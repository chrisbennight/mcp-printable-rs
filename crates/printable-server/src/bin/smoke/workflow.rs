use serde_json::{Value, json};

pub fn request(operation: &str, mut arguments: Value) -> Result<(&'static str, Value), String> {
    let (name, action) = match operation {
        "printable_status" => {
            arguments["detail"] = json!(true);
            ("status", None)
        }
        "printable_blender_execute" => ("blender_execute", None),
        "printable_validate_mesh" => ("validate_mesh", None),
        "printable_analyze_assembly" => ("analyze_assembly", None),
        "printable_compare_renders" => ("compare_renders", None),
        "printable_scene_get" => ("inspect", Some("scene")),
        "printable_object_get" => ("inspect", Some("object")),
        "printable_node_tree_get" => ("inspect", Some("node_tree")),
        "printable_editing_state_get" => ("inspect", Some("editing_state")),
        "printable_primitive_create" => ("edit", Some("primitive")),
        "printable_boolean_apply" => ("edit", Some("boolean")),
        "printable_object_rename" => ("edit", Some("rename")),
        "printable_rigid_rotation_animate" => ("edit", Some("rigid_rotation")),
        "printable_scene_clear" => ("scene", Some("clear")),
        "printable_scene_checkpoint" | "printable_blend_save" => ("scene", Some("checkpoint")),
        "printable_scene_restore" => ("scene", Some("restore")),
        "printable_stl_import" => ("scene", Some("import")),
        "printable_stl_export" => ("scene", Some("export")),
        "printable_scad_compile" => ("scad_build", Some("mesh")),
        "printable_scad_render" => ("scad_build", Some("image")),
        "printable_scad_cross_section" => ("scad_build", Some("section")),
        "printable_render_dimensions" => ("view", Some("dimensions")),
        "printable_render_cross_section" => ("view", Some("section")),
        "printable_render_printability_heatmap" => ("view", Some("overhangs")),
        "printable_native_view" => ("view", Some("native")),
        "printable_render_preview" => ("render", Some("scene")),
        "printable_render_product" => ("render", Some("product")),
        "printable_render_gallery" => ("render", Some("gallery")),
        "printable_render_turntable" => ("render", Some("turntable")),
        "printable_render_job_submit" => ("job", Some("submit")),
        "printable_render_job_status" => {
            arguments["detail"] = json!(true);
            ("job", Some("get"))
        }
        "printable_render_job_list" => ("job", Some("list")),
        "printable_render_job_artifacts" => ("job", Some("artifacts")),
        "printable_render_job_cancel" => ("job", Some("cancel")),
        "printable_workspace_list" => ("artifact", Some("list")),
        "printable_workspace_read" => ("artifact", Some("read")),
        "printable_workspace_write" => ("artifact", Some("write")),
        "printable_workspace_publish" => ("artifact", Some("publish")),
        "printable_workspace_write_begin" => ("artifact", Some("upload_begin")),
        "printable_workspace_write_chunk" => ("artifact", Some("upload_chunk")),
        "printable_workspace_write_commit" => ("artifact", Some("upload_commit")),
        _ => return Err(format!("unknown acceptance operation: {operation}")),
    };
    Ok((
        name,
        match action {
            Some(action) => json!({"action": action, "params": arguments}),
            None => arguments,
        },
    ))
}
