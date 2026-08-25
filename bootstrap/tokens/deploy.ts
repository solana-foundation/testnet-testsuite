import { readFile } from "node:fs/promises";
import {
  Connection,
  Keypair,
  PublicKey,
  sendAndConfirmTransaction,
  SystemProgram,
  Transaction,
  type TransactionInstruction,
} from "@solana/web3.js";
import {
  createInitializeMintInstruction,
  getMint,
  MINT_SIZE,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";

const TOKEN_METADATA_PROGRAM_ID = new PublicKey(
  "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s",
);
const DEFAULT_MAINNET_RPC = "https://api.mainnet-beta.solana.com";
const DEFAULT_TESTNET_RPC = "https://api.testnet.solana.com";

/** A token entry from the committed bootstrap configuration. */
export interface TokenConfig {
  readonly mainnetPubkey: string;
  readonly mint: string;
  readonly mintAuthority: string;
  readonly freezeAuthority: string;
  readonly metadataAuthority: string;
}

interface SourceMetadata {
  readonly name: string;
  readonly symbol: string;
  readonly uri: string;
  readonly sellerFeeBasisPoints: number;
  readonly isMutable: boolean;
}

interface LoadedKeypairs {
  readonly mint: Keypair;
  readonly mintAuthority: Keypair;
  readonly freezeAuthority: Keypair;
  readonly metadataAuthority: Keypair;
}

interface Options {
  readonly apply: boolean;
  readonly configPath: string;
  readonly mainnetRpc: string;
  readonly testnetRpc: string;
}

/** Parses and validates command-line options without performing network I/O. */
export function parseOptions(args: readonly string[]): Options {
  let apply = false;
  let configPath = "bootstrap/tokens/config.json";
  let mainnetRpc = DEFAULT_MAINNET_RPC;
  let testnetRpc = DEFAULT_TESTNET_RPC;
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--apply") {
      apply = true;
      continue;
    }
    const value = args[index + 1];
    if (!value) throw new Error(`Missing value for ${argument ?? "argument"}.`);
    if (argument === "--config") configPath = value;
    else if (argument === "--mainnet-rpc") mainnetRpc = value;
    else if (argument === "--testnet-rpc") testnetRpc = value;
    else throw new Error(`Unknown argument: ${argument}`);
    index += 1;
  }
  return { apply, configPath, mainnetRpc, testnetRpc };
}

/** Validates untrusted JSON configuration into a strongly typed token list. */
export function parseConfig(value: unknown): readonly TokenConfig[] {
  if (!Array.isArray(value) || value.length === 0)
    throw new Error("Token config must be a non-empty JSON array.");
  const fields = [
    "mainnetPubkey",
    "mint",
    "mintAuthority",
    "freezeAuthority",
    "metadataAuthority",
  ] as const;
  return value.map((entry, index) => {
    if (typeof entry !== "object" || entry === null || Array.isArray(entry))
      throw new Error(`Config entry ${index} must be an object.`);
    const candidate = entry as Record<string, unknown>;
    for (const field of fields) {
      if (typeof candidate[field] !== "string" || candidate[field].length === 0)
        throw new Error(
          `Config entry ${index}.${field} must be a non-empty string.`,
        );
    }
    try {
      new PublicKey(candidate.mainnetPubkey as string);
    } catch {
      throw new Error(
        `Config entry ${index}.mainnetPubkey is not a valid public key.`,
      );
    }
    return {
      mainnetPubkey: candidate.mainnetPubkey as string,
      mint: candidate.mint as string,
      mintAuthority: candidate.mintAuthority as string,
      freezeAuthority: candidate.freezeAuthority as string,
      metadataAuthority: candidate.metadataAuthority as string,
    };
  });
}

/** Decodes an injected keypair environment variable. */
function keypairFromEnvironment(variableName: string): Keypair {
  const secret = process.env[variableName];
  if (!secret)
    throw new Error(
      `Required environment variable ${variableName} is not set.`,
    );
  let parsed: unknown;
  try {
    parsed = JSON.parse(secret);
  } catch {
    throw new Error(
      `Environment variable ${variableName} must be a JSON-encoded keypair byte array.`,
    );
  }
  if (
    !Array.isArray(parsed) ||
    parsed.length !== 64 ||
    parsed.some((byte) => !Number.isInteger(byte) || byte < 0 || byte > 255)
  ) {
    throw new Error(
      `Environment variable ${variableName} must contain exactly 64 byte values.`,
    );
  }
  return Keypair.fromSecretKey(Uint8Array.from(parsed));
}

function loadKeypairs(config: TokenConfig): LoadedKeypairs {
  const names = [
    config.mint,
    config.mintAuthority,
    config.freezeAuthority,
    config.metadataAuthority,
  ] as const;
  const secrets = names.map(keypairFromEnvironment);
  const [mint, mintAuthority, freezeAuthority, metadataAuthority] = secrets;
  if (!mint || !mintAuthority || !freezeAuthority || !metadataAuthority) {
    throw new Error("Unable to load all configured Doppler keypairs.");
  }
  return { mint, mintAuthority, freezeAuthority, metadataAuthority };
}

function metadataAddress(mint: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [
      Buffer.from("metadata"),
      TOKEN_METADATA_PROGRAM_ID.toBuffer(),
      mint.toBuffer(),
    ],
    TOKEN_METADATA_PROGRAM_ID,
  )[0];
}

class Reader {
  private offset = 0;
  public constructor(private readonly data: Buffer) {}
  public u8(): number {
    this.ensure(1);
    return this.data[this.offset++]!;
  }
  public u16(): number {
    this.ensure(2);
    const value = this.data.readUInt16LE(this.offset);
    this.offset += 2;
    return value;
  }
  public u32(): number {
    this.ensure(4);
    const value = this.data.readUInt32LE(this.offset);
    this.offset += 4;
    return value;
  }
  public bytes(length: number): Buffer {
    this.ensure(length);
    const value = this.data.subarray(this.offset, this.offset + length);
    this.offset += length;
    return value;
  }
  public string(): string {
    const length = this.u32();
    return this.bytes(length).toString("utf8").replace(/\0+$/u, "");
  }
  private ensure(length: number): void {
    if (this.offset + length > this.data.length)
      throw new Error("Metadata account ended unexpectedly.");
  }
}

/** Decodes name, symbol, URI, seller fee, and mutability from Metaplex account data. */
export function decodeMetadata(data: Buffer): SourceMetadata {
  const reader = new Reader(data);
  reader.u8();
  reader.bytes(32); // update authority
  reader.bytes(32); // mint
  const name = reader.string();
  const symbol = reader.string();
  const uri = reader.string();
  const sellerFeeBasisPoints = reader.u16();
  const creatorsTag = reader.u8();
  if (creatorsTag === 1) reader.bytes(reader.u32() * 34);
  else if (creatorsTag !== 0)
    throw new Error(`Invalid creators option tag: ${creatorsTag}.`);
  reader.u8(); // primary sale happened
  const isMutable = reader.u8() === 1;
  return { name, symbol, uri, sellerFeeBasisPoints, isMutable };
}

function stringBytes(value: string): Buffer {
  const encoded = Buffer.from(value, "utf8");
  const length = Buffer.alloc(4);
  length.writeUInt32LE(encoded.length);
  return Buffer.concat([length, encoded]);
}

function createMetadataInstruction(
  mint: PublicKey,
  payer: PublicKey,
  mintAuthority: PublicKey,
  metadataAuthority: PublicKey,
  metadata: SourceMetadata,
): TransactionInstruction {
  // CreateMetadataAccountV3 with DataV2. Optional creator, collection, and use
  // fields are omitted because V3 cannot faithfully recreate their verifications.
  const sellerFee = Buffer.alloc(2);
  sellerFee.writeUInt16LE(metadata.sellerFeeBasisPoints);
  const data = Buffer.concat([
    Buffer.from([33]),
    stringBytes(metadata.name),
    stringBytes(metadata.symbol),
    stringBytes(metadata.uri),
    sellerFee,
    Buffer.from([0, 0, 0, metadata.isMutable ? 1 : 0, 0]),
  ]);
  return {
    programId: TOKEN_METADATA_PROGRAM_ID,
    keys: [
      { pubkey: metadataAddress(mint), isSigner: false, isWritable: true },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: mintAuthority, isSigner: true, isWritable: false },
      { pubkey: payer, isSigner: true, isWritable: true },
      { pubkey: metadataAuthority, isSigner: true, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
    ],
    data,
  };
}

async function deployToken(
  config: TokenConfig,
  options: Options,
): Promise<void> {
  const mainnet = new Connection(options.mainnetRpc, "confirmed");
  const testnet = new Connection(options.testnetRpc, "confirmed");
  const sourceMint = new PublicKey(config.mainnetPubkey);
  const [sourceMintInfo, sourceMetadataAccount] = await Promise.all([
    getMint(mainnet, sourceMint, "confirmed", TOKEN_PROGRAM_ID),
    mainnet.getAccountInfo(metadataAddress(sourceMint), "confirmed"),
  ]);
  if (
    !sourceMetadataAccount ||
    !sourceMetadataAccount.owner.equals(TOKEN_METADATA_PROGRAM_ID)
  )
    throw new Error(
      `No Metaplex metadata account exists for mainnet mint ${sourceMint.toBase58()}.`,
    );
  const sourceMetadata = decodeMetadata(sourceMetadataAccount.data);
  const keypairs = loadKeypairs(config);
  if (await testnet.getAccountInfo(keypairs.mint.publicKey, "confirmed"))
    throw new Error(
      `Testnet mint ${keypairs.mint.publicKey.toBase58()} already exists; refusing to overwrite it.`,
    );
  const rent = await testnet.getMinimumBalanceForRentExemption(
    MINT_SIZE,
    "confirmed",
  );
  console.log(
    JSON.stringify(
      {
        action: options.apply ? "apply" : "dry-run",
        mainnet: {
          mint: sourceMint.toBase58(),
          decimals: sourceMintInfo.decimals,
          supply: sourceMintInfo.supply.toString(),
          metadata: sourceMetadata,
        },
        testnet: {
          mint: keypairs.mint.publicKey.toBase58(),
          metadata: metadataAddress(keypairs.mint.publicKey).toBase58(),
          mintAuthority: keypairs.mintAuthority.publicKey.toBase58(),
          freezeAuthority: keypairs.freezeAuthority.publicKey.toBase58(),
          metadataAuthority: keypairs.metadataAuthority.publicKey.toBase58(),
          payer: keypairs.mintAuthority.publicKey.toBase58(),
          minimumRentLamports: rent,
          zeroSupply: true,
        },
      },
      null,
      2,
    ),
  );
  if (!options.apply) return;
  const transaction = new Transaction().add(
    SystemProgram.createAccount({
      fromPubkey: keypairs.mintAuthority.publicKey,
      newAccountPubkey: keypairs.mint.publicKey,
      lamports: rent,
      space: MINT_SIZE,
      programId: TOKEN_PROGRAM_ID,
    }),
    createInitializeMintInstruction(
      keypairs.mint.publicKey,
      sourceMintInfo.decimals,
      keypairs.mintAuthority.publicKey,
      keypairs.freezeAuthority.publicKey,
    ),
    createMetadataInstruction(
      keypairs.mint.publicKey,
      keypairs.mintAuthority.publicKey,
      keypairs.mintAuthority.publicKey,
      keypairs.metadataAuthority.publicKey,
      sourceMetadata,
    ),
  );
  const simulation = await testnet.simulateTransaction(transaction, [
    keypairs.mintAuthority,
    keypairs.mint,
    keypairs.metadataAuthority,
  ]);
  if (simulation.value.err)
    throw new Error(
      `Testnet simulation failed: ${JSON.stringify(simulation.value.err)} ${simulation.value.logs?.join(" ") ?? ""}`,
    );
  const signature = await sendAndConfirmTransaction(
    testnet,
    transaction,
    [keypairs.mintAuthority, keypairs.mint, keypairs.metadataAuthority],
    { commitment: "confirmed" },
  );
  console.log(
    JSON.stringify({
      deployed: true,
      signature,
      mint: keypairs.mint.publicKey.toBase58(),
    }),
  );
}

async function main(): Promise<void> {
  const options = parseOptions(process.argv.slice(2));
  const config = parseConfig(
    JSON.parse(await readFile(options.configPath, "utf8")),
  );
  for (const token of config) await deployToken(token, options);
}

if (process.argv[1]?.endsWith("deploy.ts")) {
  void main().catch((error: unknown) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}
