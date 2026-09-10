/// Compute the one-shot addon/client version-mismatch warning, or `None` when
/// there is nothing to warn about. Pure and never-panics. An unknown client
/// compatibility version is checked first:
///
/// - `client = None` → the client can't determine its own version, so there is
///   nothing to compare against: stay silent. This takes precedence — a `None`
///   client disables the warning even when the addon also omits its version.
/// - `addon = None` (with a known client version) → the addon predates version
///   reporting: warn.
/// - both present and equal → silent.
/// - both present and differing → warn.
pub fn format_version_mismatch(addon: Option<&str>, client: Option<&str>) -> Option<String> {
    match (addon, client) {
        (_, None) => None,
        (Some(a), Some(c)) if a == c => None,
        (Some(a), Some(c)) => Some(format!(
            "⚠️ Blender addon version {a} does not match the server's expected version {c}. \
             Reinstall the addon (printable-install-addon) and restart Blender."
        )),
        (None, Some(_)) => Some(
            "⚠️ The Blender addon predates version reporting. Reinstall the addon \
             (printable-install-addon) and restart Blender to pick up fixes the server assumes."
                .to_string(),
        ),
    }
}

/// Records the addon version seen on first contact and latches a one-shot
/// mismatch warning. `pop_warning` returns the warning exactly once.
#[derive(Debug, Default)]
pub struct VersionState {
    checked: bool,
    addon_version: Option<String>,
    pending_warning: Option<String>,
}

impl VersionState {
    /// On first contact only, record the addon version and latch the mismatch
    /// warning against `client_version`. Both the recorded version and the
    /// warning describe the version seen at first contact and stay consistent;
    /// later responses (e.g. after an addon reinstall) do not silently move the
    /// reported version away from the one the warning was computed against.
    pub fn observe(&mut self, addon_version: Option<&str>, client_version: Option<&str>) {
        if !self.checked {
            self.checked = true;
            self.pending_warning = format_version_mismatch(addon_version, client_version);
            if let Some(v) = addon_version {
                self.addon_version = Some(v.to_string());
            }
        }
    }

    pub fn addon_version(&self) -> Option<String> {
        self.addon_version.clone()
    }

    /// Return the latched warning exactly once.
    pub fn pop_warning(&mut self) -> Option<String> {
        self.pending_warning.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_are_silent() {
        assert!(format_version_mismatch(Some("0.2.5"), Some("0.2.5")).is_none());
    }

    #[test]
    fn differing_versions_warn() {
        let w = format_version_mismatch(Some("0.2.4"), Some("0.2.5")).unwrap();
        assert!(w.contains("0.2.4") && w.contains("0.2.5"));
    }

    #[test]
    fn missing_addon_version_warns() {
        assert!(format_version_mismatch(None, Some("0.2.5")).is_some());
    }

    #[test]
    fn unknown_client_version_is_silent() {
        assert!(format_version_mismatch(Some("0.2.5"), None).is_none());
    }

    #[test]
    fn both_versions_unknown_is_silent() {
        // A None client disables warnings; a missing addon version must not
        // resurrect one when there is nothing to compare against.
        assert!(format_version_mismatch(None, None).is_none());
    }

    #[test]
    fn warning_is_latched_once_and_version_recorded() {
        let mut st = VersionState::default();
        st.observe(Some("0.2.4"), Some("0.2.5"));
        // A later observation does not re-latch a new warning.
        st.observe(Some("0.2.4"), Some("0.2.5"));
        assert_eq!(st.addon_version().as_deref(), Some("0.2.4"));
        assert!(st.pop_warning().is_some());
        assert!(st.pop_warning().is_none(), "warning must be one-shot");
    }

    #[test]
    fn addon_version_is_pinned_to_first_contact() {
        let mut st = VersionState::default();
        st.observe(Some("0.2.4"), Some("0.2.4"));
        // A later response reporting a different version must not move the
        // recorded version away from what the one-shot warning was computed on.
        st.observe(Some("0.2.9"), Some("0.2.4"));
        assert_eq!(st.addon_version().as_deref(), Some("0.2.4"));
    }

    #[test]
    fn no_warning_latched_when_versions_match() {
        let mut st = VersionState::default();
        st.observe(Some("0.2.5"), Some("0.2.5"));
        assert!(st.pop_warning().is_none());
        assert_eq!(st.addon_version().as_deref(), Some("0.2.5"));
    }
}
