import { Future, ModuleParameters } from "../../types/module";
import { ExecutionState, ExecutionStateMap } from "../types/execution-state";

export interface ReconciliationFailure {
  futureId: string;
  failure: string;
}

export interface ReconciliationFutureResultSuccess {
  success: true;
}

export interface ReconciliationFutureResultFailure {
  success: false;
  failure: ReconciliationFailure;
}

export type ReconciliationFutureResult =
  | ReconciliationFutureResultSuccess
  | ReconciliationFutureResultFailure;

export interface ReconciliationResult {
  reconciliationFailures: ReconciliationFailure[];
  missingExecutedFutures: string[];
}

export interface ReconciliationContext {
  executionStateMap: ExecutionStateMap;
  deploymentParameters: { [key: string]: ModuleParameters };
  accounts: string[];
}

export type ReconciliationCheck = (
  future: Future,
  executionState: ExecutionState,
  context: ReconciliationContext
) => ReconciliationFutureResult;