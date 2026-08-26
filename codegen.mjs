// Generates the TypeScript on-chain bindings for the Archer program from the
// vendored IDL. The IDL (idl/archer_v1.json) is the single source of truth,
// produced by `shank` in the archer-v1 program repo. This script converts it to
// a Codama tree and renders a @solana/kit client into ts/src/generated.
//
// Run: node codegen.mjs   (or `bun run codegen` from ts/)
//
// CI runs this and fails if ts/src/generated differs from a fresh generation,
// so the bindings can never silently drift from the IDL.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { rootNodeFromAnchor } from "@codama/nodes-from-anchor";
import { bottomUpTransformerVisitor, createFromRoot } from "codama";
import { renderVisitor } from "@codama/renderers-js";

const here = dirname(fileURLToPath(import.meta.url));
const IDL_PATH = join(here, "idl", "archer_v1.json");
// renderVisitor writes into `<OUT_DIR>/src/generated`, so point it at ts/.
const OUT_DIR = join(here, "ts");
const GENERATED_DIR = join(OUT_DIR, "src", "generated");

function loadIdl() {
  const idl = JSON.parse(readFileSync(IDL_PATH, "utf8"));
  return patchIdl(idl);
}

// The shank IDL under-specifies a few instructions relative to what the program
// actually reads. Patch those gaps here so the generated client matches the
// on-chain behavior. Each patch is annotated with the program source that
// justifies it, so a reviewer can confirm it against archer-v1.
function patchIdl(idl) {
  // InitializeMakerBook (disc 6): the program reads an optional leading `kind`
  // byte (program/src/processor/maker/initialize_maker_book.rs: `params_data.first()`),
  // but the IDL declares `args: []`. Add the kind arg so the client can pass it.
  const initMaker = idl.instructions.find(
    (ix) => ix.discriminant?.value === 6,
  );
  if (initMaker && (initMaker.args?.length ?? 0) === 0) {
    initMaker.args = [{ name: "kind", type: "u8" }];
  }
  return idl;
}

// The anchor→Codama conversion emits the explicit IDL `discriminator` field
// twice in each account struct (once from the IDL field, once as the field
// backing the account discriminator). Left as-is, the generated decoder reads
// the 8 discriminator bytes twice and every account decode is corrupted.
// Collapse consecutive duplicate struct fields by name; the account-level
// `fieldDiscriminatorNode` still points at the single remaining field.
function dedupeStructFields() {
  return bottomUpTransformerVisitor([
    {
      select: "[structTypeNode]",
      transform: (node) => {
        const seen = new Set();
        const fields = node.fields.filter((f) => {
          if (seen.has(f.name)) return false;
          seen.add(f.name);
          return true;
        });
        return { ...node, fields };
      },
    },
  ]);
}

async function main() {
  const idl = loadIdl();
  const codama = createFromRoot(rootNodeFromAnchor(idl));
  codama.update(dedupeStructFields());
  await codama.accept(
    renderVisitor(OUT_DIR, {
      deleteFolderBeforeRendering: true,
    }),
  );
  console.log(`Generated TS bindings → ${GENERATED_DIR}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
