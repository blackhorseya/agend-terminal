//! GitHub ssh-config host-alias stripping, split out of `dispatch_hook/mod.rs`
//! to keep that grandfathered file under its `file_size_invariant` ceiling.

/// Strip an ssh-config GitHub host alias — `git@github-<x>:` or
/// `ssh://git@github-<x>/` (also `github.com-<x>`), the usual multi-account
/// convention — returning the `owner/repo(.git)` remainder. Only
/// `github-`/`github.com-` prefixed aliases qualify: an arbitrary SCP host
/// may be GitLab/Bitbucket/self-hosted, which must not be passed to `gh`
/// as a GitHub slug. Look-alikes such as `github.com.evil.com` don't match.
pub(crate) fn strip_github_ssh_alias(s: &str) -> Option<&str> {
    let (host, rest) = match s.strip_prefix("ssh://git@") {
        Some(r) => r.split_once('/')?,
        None => s.strip_prefix("git@")?.split_once(':')?,
    };
    let alias = host
        .strip_prefix("github.com-")
        .or_else(|| host.strip_prefix("github-"))?;
    if alias.is_empty() || alias.contains(['/', ':', '@']) {
        return None;
    }
    Some(rest)
}
