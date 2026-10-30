/**
 * Exit status for `skillparity run --ci`.
 * A skip is not a failure unless every task skipped and `--skip-ok` was not set.
 */
export function decideExitCode({ failed, skipped, total, ci, skipOk }) {
  if (!ci) return 0;
  if (failed > 0) return 1;
  if (total > 0 && skipped === total && !skipOk) return 1;
  return 0;
}
