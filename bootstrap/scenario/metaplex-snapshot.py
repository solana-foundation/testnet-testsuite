"""Write a Surfpool `--snapshot` file holding the Metaplex Token Metadata
program (program + programdata accounts) copied from testnet, so an
`--offline` Surfpool can still run the token bootstrap.

    python3 -I bootstrap/scenario/metaplex-snapshot.py .scenario/metaplex.json
"""
import base64, json, sys, urllib.request

PROGRAM = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"
ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58(raw):
    n, out = int.from_bytes(raw, "big"), ""
    while n:
        n, r = divmod(n, 58)
        out = ALPHABET[r] + out
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + out


def account(rpc, pubkey):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
                       "params": [pubkey, {"encoding": "base64"}]}).encode()
    request = urllib.request.Request(rpc, body, {"content-type": "application/json"})
    value = json.load(urllib.request.urlopen(request, timeout=120))["result"]["value"]
    return {"lamports": value["lamports"], "owner": value["owner"], "executable": value["executable"],
            "rentEpoch": 0, "data": value["data"][0]}


def main():
    out_path = sys.argv[1]
    rpc = sys.argv[2] if len(sys.argv) > 2 else "https://api.testnet.solana.com"
    program = account(rpc, PROGRAM)
    # Upgradeable program account: u32 tag + programdata address.
    programdata = b58(base64.b64decode(program["data"])[4:36])
    json.dump({PROGRAM: program, programdata: account(rpc, programdata)}, open(out_path, "w"))
    print(f"wrote {out_path} ({PROGRAM} + {programdata})")


if __name__ == "__main__":
    main()
