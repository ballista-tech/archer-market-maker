// Transaction assembly + send helpers. `sendInstructions` mirrors the confirmed
// send used by the owner commands (client.rs send_instructions): it prepends the
// compute-budget instructions, signs with the fee-payer signer, and confirms.

import {
  appendTransactionMessageInstructions,
  createTransactionMessage,
  getSignatureFromTransaction,
  pipe,
  sendAndConfirmTransactionFactory,
  sendTransactionWithoutConfirmingFactory,
  setTransactionMessageFeePayerSigner,
  setTransactionMessageLifetimeUsingBlockhash,
  signTransactionMessageWithSigners,
  type Instruction,
  type KeyPairSigner,
  type Rpc,
  type RpcSubscriptions,
  type Signature,
  type SolanaRpcApi,
  type SolanaRpcSubscriptionsApi,
  type Transaction,
  type TransactionWithBlockhashLifetime,
} from "@solana/kit";
import {
  getSetComputeUnitLimitInstruction,
  getSetComputeUnitPriceInstruction,
} from "@solana-program/compute-budget";

export interface SendOptions {
  priorityFeeMicroLamports?: bigint;
  computeUnitLimit?: number;
}

export interface TxClients {
  rpc: Rpc<SolanaRpcApi>;
  rpcSubscriptions: RpcSubscriptions<SolanaRpcSubscriptionsApi>;
}

// Build, sign, send, and confirm a transaction. The signer is the fee payer.
export async function sendInstructions(
  clients: TxClients,
  signer: KeyPairSigner,
  instructions: Instruction[],
  options: SendOptions = {},
): Promise<Signature> {
  const budgetIxs: Instruction[] = [];
  if (options.computeUnitLimit !== undefined) {
    budgetIxs.push(
      getSetComputeUnitLimitInstruction({ units: options.computeUnitLimit }),
    );
  }
  if (options.priorityFeeMicroLamports !== undefined) {
    budgetIxs.push(
      getSetComputeUnitPriceInstruction({
        microLamports: options.priorityFeeMicroLamports,
      }),
    );
  }

  const { value: latestBlockhash } = await clients.rpc
    .getLatestBlockhash()
    .send();

  const message = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(signer, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
    (m) =>
      appendTransactionMessageInstructions([...budgetIxs, ...instructions], m),
  );

  const signed = await signTransactionMessageWithSigners(message);
  const sendAndConfirm = sendAndConfirmTransactionFactory({
    rpc: clients.rpc,
    rpcSubscriptions: clients.rpcSubscriptions,
  });
  // The message uses a blockhash lifetime, so the signed transaction carries
  // lastValidBlockHeight at runtime; the signer's generic return widens it to
  // TransactionWithLifetime, so narrow it back for sendAndConfirm.
  const sendable = signed as typeof signed &
    Transaction &
    TransactionWithBlockhashLifetime;
  await sendAndConfirm(sendable, { commitment: "confirmed" });
  return getSignatureFromTransaction(signed);
}

export enum TxPriority {
  Normal,
  Emergency,
}

const BLOCKHASH_TTL_MS = 2000;

interface CachedBlockhash {
  blockhash: Parameters<typeof setTransactionMessageLifetimeUsingBlockhash>[0];
  fetchedAt: number;
}

// Fire-and-forget transaction sender for the engine (port of tx.rs TxSender).
// fire() builds/signs/sends in the background without awaiting, caches the
// blockhash for BLOCKHASH_TTL_MS, and tracks the shared consecutiveFailures
// counter. Shadow mode skips the send entirely.
export class TxSender {
  private cache: CachedBlockhash | undefined;
  private readonly send: ReturnType<typeof sendTransactionWithoutConfirmingFactory>;

  constructor(
    private readonly rpc: Rpc<SolanaRpcApi>,
    private readonly signer: KeyPairSigner,
    private readonly priorityFee: bigint,
    private readonly shadowMode: boolean,
    private readonly state: { consecutiveFailures: number },
  ) {
    this.send = sendTransactionWithoutConfirmingFactory({ rpc });
  }

  private async blockhash() {
    if (this.cache && nowMs() - this.cache.fetchedAt < BLOCKHASH_TTL_MS) {
      return this.cache.blockhash;
    }
    // Match tx.rs: fetch the blockhash at `processed` commitment.
    const { value } = await this.rpc
      .getLatestBlockhash({ commitment: "processed" })
      .send();
    this.cache = { blockhash: value, fetchedAt: nowMs() };
    return value;
  }

  fire(instructions: Instruction[], priority: TxPriority, cuLimit: number): void {
    if (this.shadowMode) {
      return;
    }
    const fee =
      priority === TxPriority.Emergency ? this.priorityFee * 10n : this.priorityFee;

    void this.buildAndSend(instructions, fee, cuLimit).then(
      () => {
        this.state.consecutiveFailures = 0;
      },
      () => {
        this.state.consecutiveFailures += 1;
      },
    );
  }

  private async buildAndSend(
    instructions: Instruction[],
    fee: bigint,
    cuLimit: number,
  ): Promise<void> {
    const budgetIxs: Instruction[] = [
      getSetComputeUnitLimitInstruction({ units: cuLimit }),
      getSetComputeUnitPriceInstruction({ microLamports: fee }),
    ];
    const blockhash = await this.blockhash();
    const message = pipe(
      createTransactionMessage({ version: 0 }),
      (m) => setTransactionMessageFeePayerSigner(this.signer, m),
      (m) => setTransactionMessageLifetimeUsingBlockhash(blockhash, m),
      (m) =>
        appendTransactionMessageInstructions([...budgetIxs, ...instructions], m),
    );
    const signed = await signTransactionMessageWithSigners(message);
    await this.send(signed as typeof signed & Transaction, {
      commitment: "processed",
      skipPreflight: true,
      maxRetries: 0n,
    });
  }
}

function nowMs(): number {
  return performance.timeOrigin + performance.now();
}
