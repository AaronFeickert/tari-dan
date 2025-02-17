import type { Decision } from "./Decision";
import type { Evidence } from "./Evidence";
import type { LeaderFee } from "./LeaderFee";
import type { TransactionPoolStage } from "./TransactionPoolStage";
export interface TransactionPoolRecord {
    transaction_id: string;
    evidence: Evidence;
    transaction_fee: number;
    leader_fee: LeaderFee | null;
    stage: TransactionPoolStage;
    pending_stage: TransactionPoolStage | null;
    original_decision: Decision;
    local_decision: Decision | null;
    remote_decision: Decision | null;
    is_ready: boolean;
}
