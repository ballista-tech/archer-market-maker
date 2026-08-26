// Load a Solana CLI keypair (id.json — a JSON array of 64 bytes) into a kit
// TransactionSigner. Mirrors load_keypair / read_keypair_file in main.rs.

import { readFileSync } from "node:fs";

import {
  createKeyPairSignerFromBytes,
  type KeyPairSigner,
} from "@solana/kit";

import { resolvePath } from "./config";

export async function loadKeypairSigner(path: string): Promise<KeyPairSigner> {
  const resolved = resolvePath(path);
  let raw: unknown;
  try {
    raw = JSON.parse(readFileSync(resolved, "utf8"));
  } catch (e) {
    throw new Error(
      `Failed to load keypair from ${resolved}: ${e instanceof Error ? e.message : e}`,
    );
  }
  if (!Array.isArray(raw) || raw.length !== 64) {
    throw new Error(
      `Failed to load keypair from ${resolved}: expected a 64-byte JSON array`,
    );
  }
  return createKeyPairSignerFromBytes(Uint8Array.from(raw as number[]));
}
