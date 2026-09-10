"""First-party persistent bridge for headless Blender."""

VERSION = "0.5.0"

bl_info = {
    "name": "Printable Headless Bridge",
    "author": "mcp-printable-rs contributors",
    "version": (0, 5, 0),
    "blender": (5, 1, 2),
    "location": "Background service",
    "description": "Main-thread command bridge for Printable",
    "category": "System",
}


def register() -> None:
    """Blender add-on entry point; the headless launcher owns the runtime."""


def unregister() -> None:
    """Blender add-on exit point; the headless launcher owns the runtime."""
