// Transaction assembly + send helpers. `sendInstructions` mirrors the confirmed
// send used by the owner commands (client.rs send_instructions): it prepends the
// compute-budget instructions, signs with the fee-payer signer, and confirms.

import {
  appendTransactionMessageInstructions,
  createTransactionMessage,
  getSignatureFromTransaction,
  pipe,
  sendAndConfirmTransactionFactory,
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
