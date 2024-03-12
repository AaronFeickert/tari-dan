// This file was generated by [ts-rs](https://github.com/Aleph-Alpha/ts-rs). Do not edit this file manually.
import type { Decision } from "./Decision";
import type { Evidence } from "./Evidence";
import type { LeaderFee } from "./LeaderFee";

export interface TransactionAtom {
  id: string;
  decision: Decision;
  evidence: Evidence;
  transaction_fee: number;
  leader_fee: LeaderFee | null;
}
