// Port of src/archer/client.rs — read-only RPC access to the Archer program:
// market config, market/book scans, and token-symbol resolution.

import {
  address,
  getBase58Decoder,
  getBase64Encoder,
  getProgramDerivedAddress,
  getAddressEncoder,
  getUtf8Encoder,
  type Address,
  type Base58EncodedBytes,
  type ReadonlyUint8Array,
  type Rpc,
  type SolanaRpcApi,
} from "@solana/kit";
import { TOKEN_2022_PROGRAM_ADDRESS } from "@solana-program/token-2022";
import { findAssociatedTokenPda } from "@solana-program/token";

import {
  getMakerBookDecoder,
  MAKER_BOOK_DISCRIMINATOR,
  type MakerBook,
} from "../generated/accounts/makerBook";
import {
  getMarketStateHeaderDecoder,
  MARKET_STATE_HEADER_DISCRIMINATOR,
  type MarketStateHeader,
} from "../generated/accounts/marketStateHeader";
import { ARCHER_V1_PROGRAM_ADDRESS } from "../generated/programs/archerV1";
import { MarketConfig } from "./marketConfig";
import { findMakerBookPda } from "./pda";

const METAPLEX_METADATA_PROGRAM = address(
  "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s",
);
const MAX_MULTI_ACCOUNTS = 100;

const base64 = getBase64Encoder();
const addressEncoder = getAddressEncoder();
const utf8 = getUtf8Encoder();

type ArcherRpc = Rpc<SolanaRpcApi>;

function decodeBase64(data: string): Uint8Array {
  return new Uint8Array(base64.encode(data));
}

export class ArcherClient {
  private readonly rpc: ArcherRpc;

  constructor(rpc: ArcherRpc) {
    this.rpc = rpc;
  }

  async getMarketConfig(market: Address): Promise<MarketConfig> {
    const { value } = await this.rpc
      .getAccountInfo(market, { encoding: "base64" })
      .send();
    if (!value) {
      throw new Error("Failed to fetch market account");
    }
    const header = getMarketStateHeaderDecoder().decode(
      decodeBase64(value.data[0]),
    );

    const [baseMintAcc, quoteMintAcc] = await Promise.all([
      this.rpc.getAccountInfo(header.baseMint, { encoding: "base64" }).send(),
      this.rpc.getAccountInfo(header.quoteMint, { encoding: "base64" }).send(),
    ]);
    if (!baseMintAcc.value || !quoteMintAcc.value) {
      throw new Error("Failed to fetch mint account");
    }

    // Mint decimals byte lives at offset 44 in both token programs.
    const baseDecimals = decodeBase64(baseMintAcc.value.data[0])[44]!;
    const quoteDecimals = decodeBase64(quoteMintAcc.value.data[0])[44]!;
    const baseTokenProgram = baseMintAcc.value.owner;
    const quoteTokenProgram = quoteMintAcc.value.owner;

    const [baseVault] = await findAssociatedTokenPda({
      owner: market,
      mint: header.baseMint,
      tokenProgram: baseTokenProgram,
    });
    const [quoteVault] = await findAssociatedTokenPda({
      owner: market,
      mint: header.quoteMint,
      tokenProgram: quoteTokenProgram,
    });

    return new MarketConfig({
      marketPubkey: market,
      header,
      baseDecimals,
      quoteDecimals,
      baseTokenProgram,
      quoteTokenProgram,
      baseVault,
      quoteVault,
    });
  }

  async getAllMarkets(): Promise<[Address, MarketStateHeader][]> {
    const accounts = await this.rpc
      .getProgramAccounts(ARCHER_V1_PROGRAM_ADDRESS, {
        encoding: "base64",
        filters: [{ memcmp: memcmpDiscriminator(0, MARKET_STATE_HEADER_DISCRIMINATOR) }],
      })
      .send();

    const out: [Address, MarketStateHeader][] = [];
    const decoder = getMarketStateHeaderDecoder();
    for (const { pubkey, account } of accounts) {
      try {
        out.push([pubkey, decoder.decode(decodeBase64(account.data[0]))]);
      } catch {
        // skip accounts that fail to decode, like the Rust filter_map
      }
    }
    return out;
  }

  async getMakerBooksForMarket(market: Address): Promise<MakerBook[]> {
    const accounts = await this.rpc
      .getProgramAccounts(ARCHER_V1_PROGRAM_ADDRESS, {
        encoding: "base64",
        filters: [
          { memcmp: memcmpDiscriminator(0, MAKER_BOOK_DISCRIMINATOR) },
          { memcmp: memcmpAddress(40, market) },
        ],
      })
      .send();

    const out: MakerBook[] = [];
    const decoder = getMakerBookDecoder();
    for (const { account } of accounts) {
      try {
        out.push(decoder.decode(decodeBase64(account.data[0])));
      } catch {
        // skip
      }
    }
    return out;
  }

  async getMakerBook(market: Address, maker: Address): Promise<MakerBook> {
    const [pda] = await findMakerBookPda(market, maker);
    const { value } = await this.rpc
      .getAccountInfo(pda, { encoding: "base64" })
      .send();
    if (!value) {
      throw new Error("Failed to fetch maker book account");
    }
    return getMakerBookDecoder().decode(decodeBase64(value.data[0]));
  }

  // Resolve symbols for a set of mints: try Metaplex metadata first, then fall
  // back to Token-2022 on-mint metadata for anything still missing.
  async getTokenSymbols(mints: Address[]): Promise<Map<Address, string>> {
    const out = new Map<Address, string>();
    if (mints.length === 0) {
      return out;
    }

    const pdas = await Promise.all(mints.map(metaplexMetadataPda));
    for (let i = 0; i < mints.length; i += MAX_MULTI_ACCOUNTS) {
      const mintChunk = mints.slice(i, i + MAX_MULTI_ACCOUNTS);
      const pdaChunk = pdas.slice(i, i + MAX_MULTI_ACCOUNTS);
      let accounts;
      try {
        accounts = (
          await this.rpc
            .getMultipleAccounts(pdaChunk, { encoding: "base64" })
            .send()
        ).value;
      } catch {
        continue;
      }
      mintChunk.forEach((mint, j) => {
        const acc = accounts[j];
        if (acc) {
          const sym = parseMetaplexSymbol(decodeBase64(acc.data[0]));
          if (sym) out.set(mint, sym);
        }
      });
    }

    const missing = mints.filter((m) => !out.has(m));
    for (let i = 0; i < missing.length; i += MAX_MULTI_ACCOUNTS) {
      const chunk = missing.slice(i, i + MAX_MULTI_ACCOUNTS);
      let accounts;
      try {
        accounts = (
          await this.rpc.getMultipleAccounts(chunk, { encoding: "base64" }).send()
        ).value;
      } catch {
        continue;
      }
      chunk.forEach((mint, j) => {
        const acc = accounts[j];
        if (acc && acc.owner === TOKEN_2022_PROGRAM_ADDRESS) {
          const sym = parseToken2022Symbol(decodeBase64(acc.data[0]));
          if (sym) out.set(mint, sym);
        }
      });
    }

    return out;
  }
}

function memcmpDiscriminator(offset: number, disc: ReadonlyUint8Array) {
  return {
    offset: BigInt(offset),
    bytes: bytesToBase58(disc),
    encoding: "base58" as const,
  };
}

function memcmpAddress(offset: number, addr: Address) {
  return {
    offset: BigInt(offset),
    bytes: bytesToBase58(new Uint8Array(addressEncoder.encode(addr))),
    encoding: "base58" as const,
  };
}

const base58 = getBase58Decoder();
function bytesToBase58(bytes: ReadonlyUint8Array): Base58EncodedBytes {
  return base58.decode(bytes) as Base58EncodedBytes;
}

async function metaplexMetadataPda(mint: Address): Promise<Address> {
  const [pda] = await getProgramDerivedAddress({
    programAddress: METAPLEX_METADATA_PROGRAM,
    seeds: [
      utf8.encode("metadata"),
      addressEncoder.encode(METAPLEX_METADATA_PROGRAM),
      addressEncoder.encode(mint),
    ],
  });
  return pda;
}

function readU32(data: Uint8Array, offset: number): number | undefined {
  if (offset + 4 > data.length) return undefined;
  return (
    data[offset]! |
    (data[offset + 1]! << 8) |
    (data[offset + 2]! << 16) |
    (data[offset + 3]! << 24)
  ) >>> 0;
}

// Borsh string: u32 length prefix + bytes. Returns [trimmed, endOffset].
function readBorshString(
  data: Uint8Array,
  offset: number,
): [string, number] | undefined {
  const len = readU32(data, offset);
  if (len === undefined) return undefined;
  const start = offset + 4;
  const end = start + len;
  if (end > data.length) return undefined;
  const raw = new TextDecoder().decode(data.subarray(start, end));
  // Trim NUL bytes from the ends only (matching Rust trim_matches('\0')), then
  // trim surrounding whitespace.
  const trimmed = raw.replace(/^\0+|\0+$/g, "").trim();
  return [trimmed, end];
}

function parseMetaplexSymbol(data: Uint8Array): string | undefined {
  const afterName = readBorshString(data, 65);
  if (!afterName) return undefined;
  const sym = readBorshString(data, afterName[1]);
  if (!sym || sym[0].length === 0) return undefined;
  return sym[0];
}

function parseToken2022Symbol(data: Uint8Array): string | undefined {
  const TLV_START = 166;
  const TOKEN_METADATA_TYPE = 19;

  let offset = TLV_START;
  while (offset + 4 <= data.length) {
    const extType = data[offset]! | (data[offset + 1]! << 8);
    const extLen = data[offset + 2]! | (data[offset + 3]! << 8);
    if (extType === 0) break;
    const valStart = offset + 4;
    const valEnd = valStart + extLen;
    if (valEnd > data.length) break;
    if (extType === TOKEN_METADATA_TYPE) {
      const val = data.subarray(valStart, valEnd);
      const afterName = readBorshString(val, 64);
      if (!afterName) return undefined;
      const sym = readBorshString(val, afterName[1]);
      if (!sym || sym[0].length === 0) return undefined;
      return sym[0];
    }
    offset = valEnd;
  }
  return undefined;
}
