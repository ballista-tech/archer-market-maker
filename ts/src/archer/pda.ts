// Program-derived addresses for the Archer program. Codama did not render seed
// helpers from the shank IDL, so these are hand-written to match the Rust seeds
// (src/archer/types.rs: MakerBook / MakerRegistry get_address).

import {
  getAddressEncoder,
  getProgramDerivedAddress,
  getUtf8Encoder,
  type Address,
  type ProgramDerivedAddress,
} from "@solana/kit";

import { ARCHER_V1_PROGRAM_ADDRESS } from "../generated/programs/archerV1";

const addressEncoder = getAddressEncoder();
const utf8 = getUtf8Encoder();

// seeds = ["maker", market, maker]
export async function findMakerBookPda(
  market: Address,
  maker: Address,
): Promise<ProgramDerivedAddress> {
  return getProgramDerivedAddress({
    programAddress: ARCHER_V1_PROGRAM_ADDRESS,
    seeds: [
      utf8.encode("maker"),
      addressEncoder.encode(market),
      addressEncoder.encode(maker),
    ],
  });
}

// seeds = ["maker_registry", market]
export async function findMakerRegistryPda(
  market: Address,
): Promise<ProgramDerivedAddress> {
  return getProgramDerivedAddress({
    programAddress: ARCHER_V1_PROGRAM_ADDRESS,
    seeds: [utf8.encode("maker_registry"), addressEncoder.encode(market)],
  });
}
