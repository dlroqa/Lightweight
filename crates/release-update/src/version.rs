//! Release version ordering.

use std::cmp::Ordering;

/// Compare dotted numeric versions, ignoring a leading `v` and any non-numeric
/// suffix on a component. Missing trailing components count as zero.
pub(crate) fn compare_versions(left: &str, right: &str) -> Ordering {
    let mut left = version_parts(left).into_iter();
    let mut right = version_parts(right).into_iter();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (Some(a), Some(b)) if a != b => return a.cmp(&b),
            (Some(_), Some(_)) => {}
            (Some(a), None) => return a.cmp(&0),
            (None, Some(b)) => return 0.cmp(&b),
        }
    }
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .trim_start_matches('v')
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions_compare_numerically() {
        assert_eq!(compare_versions("0.3.5", "0.3.4"), Ordering::Greater);
        assert_eq!(compare_versions("0.3.10", "0.3.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "1.0"), Ordering::Equal);
        assert_eq!(compare_versions("v0.3.22", "0.3.22"), Ordering::Equal);
        assert_eq!(compare_versions("0.3.5", "0.4.0"), Ordering::Less);
    }
}
