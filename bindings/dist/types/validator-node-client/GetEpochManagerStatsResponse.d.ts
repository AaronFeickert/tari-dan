import type { CommitteeInfo } from "../CommitteeInfo";
import type { Epoch } from "../Epoch";
export interface GetEpochManagerStatsResponse {
    current_epoch: Epoch;
    current_block_height: number;
    current_block_hash: string;
    is_valid: boolean;
    start_epoch: Epoch | null;
    committee_info: CommitteeInfo | null;
}
