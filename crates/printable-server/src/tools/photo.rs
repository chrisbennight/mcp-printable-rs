//! The `photo` workflow: estimates from a workspace photograph, made by the
//! photograph worker on its GPU.

use super::*;

/// Estimate relative depth, and optionally surface normals, for a workspace
/// photograph with Marigold V2.
pub(super) async fn depth(
    workspace: Arc<Workspace>,
    settings: &Settings,
    request: crate::recon::DepthRequest,
) -> Result<Value, ToolError> {
    request.validate()?;
    let client = settings
        .recon
        .as_ref()
        .ok_or_else(|| ToolError::Validation("the photograph worker is not configured".into()))?;
    workspace.stat_artifact(&request.image)?;
    validate_scad_destination(&workspace, &request.depth_path, ".png", request.overwrite)?;
    if let Some(normals) = &request.normals_path {
        validate_scad_destination(&workspace, normals, ".png", request.overwrite)?;
        if workspace.resolve(normals, false)? == workspace.resolve(&request.depth_path, false)? {
            return Err(ToolError::Validation(
                "normals_path and depth_path name the same file".into(),
            ));
        }
    }
    client.estimate_depth(&request).await
}
