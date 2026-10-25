# The matching runtime image set that CI tests and publishes, and the external
# smoke driver, built together so the shared Rust stage compiles once.
#
# CACHE_MODE selects the GitHub Actions layer cache: "read" reuses layers that
# main stored, "write" builds without reading the cache and stores the result,
# and an empty value neither reads nor writes. Published images therefore never
# reuse cached package installs.

variable "SOURCE_REVISION" {
  default = "unknown"
}

variable "CACHE_MODE" {
  default = ""
}

function "cache_from" {
  params = [scopes]
  result = CACHE_MODE == "read" ? [for scope in scopes : "type=gha,scope=${scope}"] : []
}

# "min" stores the exported image layers, which is enough when nothing in a
# stage changed. "max" also stores intermediate stages.
function "cache_to" {
  params = [scope, mode]
  result = CACHE_MODE == "write" ? ["type=gha,scope=${scope},mode=${mode}"] : []
}

group "default" {
  targets = ["server", "smoke", "blender", "cad", "slicer", "recon"]
}

target "image" {
  platforms = ["linux/amd64"]
  args = {
    SOURCE_REVISION = SOURCE_REVISION
  }
  output = ["type=docker"]
}

# The server cache keeps the Rust build stage that every worker image and the
# smoke driver copy binaries from.
target "server" {
  inherits   = ["image"]
  target     = "runtime"
  tags       = ["printable-server:ci"]
  cache-from = cache_from(["server"])
  cache-to   = cache_to("server", "max")
}

target "smoke" {
  inherits   = ["image"]
  target     = "smoke-export"
  output     = ["type=local,dest=target/release"]
  cache-from = cache_from(["smoke", "server"])
  cache-to   = cache_to("smoke", "min")
}

target "blender" {
  inherits   = ["image"]
  dockerfile = "blender/Dockerfile"
  tags       = ["printable-blender:ci"]
  cache-from = cache_from(["blender"])
  cache-to   = cache_to("blender", "min")
}

target "cad" {
  inherits   = ["image"]
  target     = "cad-runtime"
  tags       = ["printable-cad:ci"]
  cache-from = cache_from(["cad", "server"])
  cache-to   = cache_to("cad", "min")
}

target "slicer" {
  inherits   = ["image"]
  target     = "slicer-runtime"
  tags       = ["printable-slicer:ci"]
  cache-from = cache_from(["slicer", "server"])
  cache-to   = cache_to("slicer", "min")
}

target "recon" {
  inherits   = ["image"]
  target     = "recon-runtime"
  tags       = ["printable-recon:ci"]
  cache-from = cache_from(["recon", "server"])
  cache-to   = cache_to("recon", "min")
}
