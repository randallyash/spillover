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

        // Either there is work waiting, or the newest thing here is the release
        // the binary reports. Demanding an `Unreleased` section outright was the
        // earlier spelling of this, and it is unsatisfiable in exactly the place
        // it matters most: the moment a release is cut, there is genuinely
        // nothing pending, so the only way to satisfy it would be a placeholder.
        assert!(
            releases
                .iter()
                .any(|(name, _)| name.contains("Unreleased") || name.contains(crate_version())),
            "the changelog is neither ahead of nor level with the binary: {releases:?}"
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

    // ---- the version, said the same way everywhere ------------------------

    /// The version the binary reports, and the one a release is tagged with.
    fn crate_version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The version a `PKGBUILD` declares, if it declares one at all.
    fn pkgver(text: &str) -> Option<String> {
        text.lines()
            .find_map(|line| line.strip_prefix("pkgver="))
            .map(|value| value.trim().to_string())
    }

    /// The newest released version in the changelog, `Unreleased` aside.
    fn newest_release() -> Option<String> {
        include_str!("../CHANGELOG.md")
            .lines()
            .filter_map(|line| line.strip_prefix("## "))
            .find_map(|heading| {
                let heading = heading.trim();
                if heading.contains("Unreleased") {
                    return None;
                }
                Some(heading.strip_prefix('[')?.split(']').next()?.to_string())
            })
    }

    #[test]
    fn every_manifest_names_the_version_the_binary_reports() {
        // `spill --version` is the one figure a person can hold against the
        // release tag, so anything left behind at the old number reads as an
        // abandoned project. The AUR recipe sat at 0.1.1 while the crate was
        // 0.1.2, and nothing noticed, because nothing was looking — which is the
        // whole cost of a version kept in more than one place.
        let version = crate_version();

        let pkgbuild = include_str!("../packaging/arch/PKGBUILD");
        let declared = pkgver(pkgbuild);
        assert_eq!(
            declared.as_deref(),
            Some(version),
            "the AUR recipe declares {declared:?} where the binary reports {version}"
        );

        let released = newest_release();
        assert_eq!(
            released.as_deref(),
            Some(version),
            "the changelog's newest release is {released:?} where the binary reports {version}"
        );
    }

    #[test]
    fn the_readme_shows_the_version_it_would_print() {
        // The doctor sample is the first output a reader sees, and it carried
        // 0.1.0 through two releases: a screenshot of an older binary says more
        // about how maintained something is than any wording around it.
        let expected = format!("spill {} — doctor", crate_version());
        assert!(
            include_str!("../README.md").contains(&expected),
            "the README's doctor sample does not start with {expected:?}"
        );
    }
}
