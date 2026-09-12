//! The repository's own documents, checked.
//!
//! Not documentation *of* the code — the things a reader is told, verified
//! against what the program does. A changelog with two `### Added` sections in
//! one release reads as a mistake and is one, and it is the kind of mistake that
//! survives review because it looks like content. The samples that `/why` and
//! the spill log put in the README are pinned where they are generated, in
//! `stalls`, because that is where the formats live; what is left here is the
//! shape of the files themselves.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    /// Every release in the changelog, with the section kinds it declares.
    ///
    /// A section is a `## ` heading; the kinds under it are its `### ` headings.
    fn sections() -> Vec<(String, Vec<String>)> {
        let text = include_str!("../CHANGELOG.md");
        let mut releases: Vec<(String, Vec<String>)> = Vec::new();

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("## ") {
                releases.push((rest.trim().to_string(), Vec::new()));
            } else if let Some(rest) = line.strip_prefix("### ") {
                if let Some((_, kinds)) = releases.last_mut() {
                    kinds.push(rest.trim().to_string());
                }
            }
        }

        releases
    }

    #[test]
    fn the_changelog_was_found_and_has_releases() {
        // Guards the parser above: a change to the heading style would make
        // every other assertion here vacuously true.
        let releases = sections();
        assert!(releases.len() >= 3, "parsed {} releases", releases.len());
        assert!(
            releases.iter().any(|(name, _)| name.contains("Unreleased")),
            "{releases:?}"
        );
    }

    #[test]
    fn no_release_declares_the_same_section_twice() {
        // Keep a Changelog's whole value is being scannable, and two headings
        // of one kind in one release is the failure that keeps happening: an
        // entry gets appended under a fresh heading instead of the existing one,
        // which reads as deliberate and is not.
        for (release, kinds) in sections() {
            let unique: BTreeSet<&String> = kinds.iter().collect();
            assert_eq!(
                unique.len(),
                kinds.len(),
                "{release} declares a section more than once: {kinds:?}"
            );
        }
    }

    #[test]
    fn every_release_says_something() {
        // A release heading with nothing under it — or one whose only content is
        // a stray heading — is a placeholder nobody finished. `Unreleased` is
        // included: it should not be left behind empty after a release is cut.
        let text = include_str!("../CHANGELOG.md");
        for (release, kinds) in sections() {
            assert!(!kinds.is_empty(), "{release} has no sections at all");
            // The heading has to be followed by at least one entry.
            let after = text
                .split(&format!("## {release}"))
                .nth(1)
                .expect("the release was found a moment ago");
            let body: String = after
                .lines()
                .take_while(|line| !line.starts_with("## "))
                .filter(|line| line.starts_with("- "))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                body.lines().count() > 0,
                "{release} declares {kinds:?} but lists no entries"
            );
        }
    }

    #[test]
    fn the_heading_spellings_are_recognised() {
        // The parser only knows the words it is given, and the file already uses
        // a fixed set. A new one is fine; a typo is not, and this is where a
        // misspelling would show up rather than as a silently ignored section.
        let known = [
            "Added",
            "Changed",
            "Deprecated",
            "Removed",
            "Fixed",
            "Security",
        ];
        for (release, kinds) in sections() {
            for kind in kinds {
                assert!(
                    known.contains(&kind.as_str()),
                    "{release} has a section called {kind:?}, which is not one of the \
                     six Keep a Changelog kinds"
                );
            }
        }
    }
}
