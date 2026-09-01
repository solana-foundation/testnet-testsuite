// Generates Rust SDKs from program IDLs into crates/sdks/*/src/generated.
// Run via `just gen-sdks` (or `pnpm run generate` in this directory).
//
// Design: docs/dex-integration.md — SDKs are codama-generated, committed, and
// depend only on crates we pin ourselves. No anchor-lang, no upstream client
// crates.

import { createFromRoot } from "codama";
import { rootNodeFromAnchor } from "@codama/nodes-from-anchor";
import { renderVisitor } from "@codama/renderers-rust";
import { readFileSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";

const here = (rel) => fileURLToPath(new URL(rel, import.meta.url));

// `crate` is the SDK crate root; the renderer writes src/generated inside it.
const TARGETS = [
  {
    idl: "../crates/sdks/raydium-cpmm/idl/raydium_cp_swap.json",
    crate: "../crates/sdks/raydium-cpmm",
  },
  {
    idl: "../crates/sdks/manifest/idl/manifest.json",
    crate: "../crates/sdks/manifest",
  },
];

// Upstream IDL fixups, applied before conversion:
// - manifest: the instructions array contains a duplicate SwapV2 entry, and
//   the *Atoms* single-field wrapper structs are declared as accounts (shank
//   annotation artifact) while field references point at defined types.
function fixupIdl(idl) {
  if (Array.isArray(idl.instructions)) {
    // Keep the LAST occurrence of a duplicated name: manifest's IDL lists
    // SwapV2 twice, first with a bogus discriminant (4, colliding with Swap)
    // and then with the correct one (13, matching the program source).
    const lastIndex = new Map(idl.instructions.map((ix, i) => [ix.name, i]));
    idl.instructions = idl.instructions.filter((ix, i) => {
      if (lastIndex.get(ix.name) !== i) {
        console.log(
          `  fixup: dropping duplicate instruction ${ix.name} (discriminant ${ix.discriminant?.value})`,
        );
        return false;
      }
      return true;
    });
    const byDisc = new Map();
    for (const ix of idl.instructions) {
      const d = ix.discriminant?.value;
      if (d === undefined) continue;
      if (byDisc.has(d)) {
        console.warn(`  WARNING: ${ix.name} and ${byDisc.get(d)} share discriminant ${d}`);
      }
      byDisc.set(d, ix.name);
    }
  }
  if (Array.isArray(idl.accounts) && Array.isArray(idl.types)) {
    const isWrapperType = (a) => /Atoms/.test(a.name);
    for (const account of idl.accounts.filter(isWrapperType)) {
      console.log(`  fixup: reclassifying account ${account.name} as type`);
      idl.types.push(account);
    }
    idl.accounts = idl.accounts.filter((a) => !isWrapperType(a));
  }
  return idl;
}

for (const target of TARGETS) {
  const idl = fixupIdl(JSON.parse(readFileSync(here(target.idl), "utf8")));
  const name = idl.metadata?.name ?? idl.name;
  console.log(`generating ${name} -> ${target.crate}/src/generated`);
  rmSync(here(`${target.crate}/src/generated`), { recursive: true, force: true });
  const codama = createFromRoot(rootNodeFromAnchor(idl));
  codama.accept(
    renderVisitor(here(target.crate), {
      formatCode: true,
      crateFolder: here(target.crate),
      renderParentInstructions: true,
    }),
  );
}
console.log("done");
