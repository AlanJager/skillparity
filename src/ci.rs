/// Exit status for `skillparity run --ci`.
/// A skip is not a failure unless every task skipped and `--skip-ok` was not set.
pub fn decide_exit_code(
    failed: usize,
    skipped: usize,
    total: usize,
    ci: bool,
    skip_ok: bool,
) -> i32 {
    if !ci {
        return 0;
    }
    if failed > 0 {
        return 1;
    }
    if total > 0 && skipped == total && !skip_ok {
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::decide_exit_code;

    #[test]
    fn decide_exit_code_matches_node() {
        assert_eq!(decide_exit_code(0, 0, 3, true, false), 0);
        assert_eq!(decide_exit_code(1, 0, 3, true, true), 1);
        assert_eq!(decide_exit_code(0, 3, 3, true, false), 1);
        assert_eq!(decide_exit_code(0, 3, 3, true, true), 0);
        assert_eq!(decide_exit_code(2, 0, 2, false, false), 0);
    }
}
