// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A bundle's SBOM (ADR 0222): `sbom.spdx`, an SPDX 2.3 document in tag-value
//! form, as /system's own license manifest is. A publisher writes theirs,
//! listing the libraries the bundle carries, so that the catalog finds
//! their flaws; `bundle sign` writes one from the manifest where there is
//! none, which lists the bundle alone and says so.

use std::fmt::Write as _;

use manifest::bundle::Manifest;

use super::sums;

/// The SBOM's name in a bundle.
pub const SBOM: &str = "sbom.spdx";
/// The biggest SBOM a bundle carries.
pub const SBOM_MAX: u64 = 16 << 20;

/// An SBOM for `manifest`'s bundle, made at `created` (an RFC 3339 time).
pub fn write(manifest: &Manifest, created: &str) -> String {
    let id = &manifest.id.0;
    let version = &manifest.version.0;
    let publisher = one_line(&manifest.publisher.name.0);
    let digest = sums::hex(&sums::sha256(format!("{id} {version} {publisher}").as_bytes()));
    let mut o = String::new();
    let _ = writeln!(o, "SPDXVersion: SPDX-2.3");
    let _ = writeln!(o, "DataLicense: CC0-1.0");
    let _ = writeln!(o, "SPDXID: SPDXRef-DOCUMENT");
    let _ = writeln!(o, "DocumentName: {id}-{version}");
    let _ = writeln!(
        o,
        "DocumentNamespace: https://dukovo.org/spdx/bundles/{id}-{version}-{}",
        &digest[..16]
    );
    let _ = writeln!(o, "Creator: Tool: bundle-{}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(o, "Creator: Organization: {publisher}");
    let _ = writeln!(o, "Created: {created}");
    let _ = writeln!(o);
    let _ = writeln!(o, "PackageName: {id}");
    let _ = writeln!(o, "SPDXID: SPDXRef-Package-bundle");
    let _ = writeln!(o, "PackageVersion: {version}");
    let _ = writeln!(o, "PackageSupplier: Organization: {publisher}");
    let download = manifest.source.as_ref().map_or("NOASSERTION", |url| url.0.as_str());
    let _ = writeln!(o, "PackageDownloadLocation: {download}");
    if let Some(home) = &manifest.homepage {
        let _ = writeln!(o, "PackageHomePage: {}", home.0);
    }
    let _ = writeln!(o, "FilesAnalyzed: false");
    let _ = writeln!(o, "PackageLicenseConcluded: NOASSERTION");
    let _ = writeln!(o, "PackageLicenseDeclared: {}", manifest.license.0);
    let _ = writeln!(o, "PackageCopyrightText: NOASSERTION");
    let _ = writeln!(
        o,
        "PackageComment: <text>Written by bundle sign from manifest.json: it lists the bundle \
         alone. List the libraries the bundle carries as packages of their own, so that their \
         flaws are found.</text>"
    );
    let _ = writeln!(o, "Relationship: SPDXRef-DOCUMENT DESCRIBES SPDXRef-Package-bundle");
    // A license of the publisher's own is defined here, as SPDX asks.
    let mut refs: Vec<&str> = manifest
        .license
        .0
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|word| word.starts_with("LicenseRef-"))
        .collect();
    refs.sort_unstable();
    refs.dedup();
    for license in refs {
        let _ = writeln!(o);
        let _ = writeln!(o, "LicenseID: {license}");
        let _ = writeln!(o, "LicenseName: {}", &license["LicenseRef-".len()..]);
        let _ = writeln!(
            o,
            "ExtractedText: <text>The publisher's own terms, which come with the bundle.</text>"
        );
    }
    o
}

/// `text` on one line, as a tag's value is.
fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Checks a bundle's SBOM: SPDX tag-value, of SPDX 2, describing at least
/// one package.
pub fn check(text: &str) -> Result<(), String> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    match lines.next() {
        Some(first) if first.starts_with("SPDXVersion: SPDX-2.") => {}
        _ => {
            return Err(format!(
                "{SBOM}: not an SPDX 2 document: its first tag is not SPDXVersion"
            ));
        }
    }
    if !text.lines().any(|line| line.starts_with("PackageName: ")) {
        return Err(format!("{SBOM}: it lists no package"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sbom_describes_the_bundle() {
        let manifest = manifest::bundle::from_json(&manifest::bundle::template(
            "org.example.hello",
            "Hello",
            "Example, Inc.",
            "MIT OR LicenseRef-Example",
        ))
        .unwrap();
        let text = write(&manifest, "2026-10-31T12:00:00Z");
        check(&text).unwrap();
        for line in [
            "SPDXVersion: SPDX-2.3\n",
            "DocumentName: org.example.hello-0.1.0\n",
            "Creator: Organization: Example, Inc.\n",
            "Created: 2026-10-31T12:00:00Z\n",
            "PackageName: org.example.hello\n",
            "PackageVersion: 0.1.0\n",
            "PackageDownloadLocation: NOASSERTION\n",
            "PackageLicenseDeclared: MIT OR LicenseRef-Example\n",
            "Relationship: SPDXRef-DOCUMENT DESCRIBES SPDXRef-Package-bundle\n",
            "LicenseID: LicenseRef-Example\n",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        assert!(check("PackageName: x\n").is_err());
        assert!(check("SPDXVersion: SPDX-2.3\nDataLicense: CC0-1.0\n").is_err());
    }
}
