//! Model identifiers from bare IDs or pasted model page links.

use url::Url;

use crate::{LibraryError, Site};

const MAXIMUM_ID_DIGITS: usize = 20;

/// Extract a site's numeric model ID from a bare ID or a model page link.
///
/// Accepted links are `printables.com/model/<id>-…`,
/// `makerworld.com/<language>/models/<id>-…` and `thingiverse.com/thing:<id>`.
/// Thingiverse also accepts a bare `thing:<id>`. Query strings and fragments
/// are ignored.
///
/// # Errors
///
/// Returns [`LibraryError::InvalidRequest`] when the value is neither.
pub fn model_id(site: Site, reference: &str) -> Result<String, LibraryError> {
    let reference = reference.trim();
    let bare = match site {
        Site::Thingiverse => reference.strip_prefix("thing:").unwrap_or(reference),
        Site::MakerWorld | Site::Printables => reference,
    };
    if numeric(bare) {
        return Ok(bare.to_owned());
    }
    let invalid = || {
        LibraryError::InvalidRequest(format!(
            "model must be a numeric {site} model ID or a {site} model page link"
        ))
    };
    let url = Url::parse(reference).map_err(|_| invalid())?;
    let host = url.host_str().unwrap_or_default();
    let host = host.strip_prefix("www.").unwrap_or(host);
    if !matches!(url.scheme(), "http" | "https") || host != site_host(site) {
        return Err(invalid());
    }
    let mut segments = url.path_segments().into_iter().flatten();
    let id = match site {
        Site::Thingiverse => segments
            .find_map(|segment| segment.strip_prefix("thing:"))
            .map(leading_digits),
        Site::Printables => segments
            .skip_while(|segment| *segment != "model")
            .nth(1)
            .map(leading_digits),
        Site::MakerWorld => segments
            .skip_while(|segment| *segment != "models")
            .nth(1)
            .map(leading_digits),
    };
    id.filter(|id| numeric(id))
        .map(str::to_owned)
        .ok_or_else(invalid)
}

fn site_host(site: Site) -> &'static str {
    match site {
        Site::MakerWorld => "makerworld.com",
        Site::Printables => "printables.com",
        Site::Thingiverse => "thingiverse.com",
    }
}

fn leading_digits(segment: &str) -> &str {
    let end = segment
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(segment.len());
    &segment[..end]
}

pub(crate) fn numeric(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_ID_DIGITS
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_come_from_bare_ids_and_site_links() {
        for (site, reference, expected) in [
            (Site::Printables, "553277", "553277"),
            (
                Site::Printables,
                "https://www.printables.com/model/553277-low-poly-tray-bowl",
                "553277",
            ),
            (
                Site::Printables,
                "https://printables.com/model/553277/files?lang=en",
                "553277",
            ),
            (
                Site::MakerWorld,
                "https://makerworld.com/en/models/1514079-mesh-bowl#profileId-1580000",
                "1514079",
            ),
            (
                Site::MakerWorld,
                "https://makerworld.com/models/1514079",
                "1514079",
            ),
            (Site::Thingiverse, "thing:763622", "763622"),
            (
                Site::Thingiverse,
                "https://www.thingiverse.com/thing:763622/files",
                "763622",
            ),
        ] {
            assert_eq!(model_id(site, reference).unwrap(), expected, "{reference}");
        }
    }

    #[test]
    fn foreign_hosts_and_malformed_references_are_refused() {
        for (site, reference) in [
            (
                Site::Printables,
                "https://evil.example/model/553277-low-poly",
            ),
            (
                Site::Printables,
                "https://printables.com.evil.example/model/1",
            ),
            (Site::MakerWorld, "https://www.printables.com/model/553277"),
            (Site::MakerWorld, "file:///makerworld.com/models/1"),
            (Site::Thingiverse, "thing:abc"),
            (Site::Printables, "-1"),
            (Site::Printables, "123456789012345678901"),
            (
                Site::Printables,
                "https://www.printables.com/model/slug-only",
            ),
            (Site::MakerWorld, ""),
        ] {
            assert!(
                matches!(
                    model_id(site, reference),
                    Err(LibraryError::InvalidRequest(_))
                ),
                "{reference}"
            );
        }
    }
}
