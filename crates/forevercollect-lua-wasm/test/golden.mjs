// Checks the WASM build in pkg/ against the parser's golden fixture: the browser must
// produce exactly the snapshot the desktop uploader and the Go ingress produced.
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"

const here = (path) => fileURLToPath(new URL(path, import.meta.url))
const fixtures = "../../forevercollect-lua/tests/fixtures/"

const { initSync, parse } = await import(here("../pkg/forevercollect_lua_wasm.js"))
initSync({ module: readFileSync(here("../pkg/forevercollect_lua_wasm_bg.wasm")) })

// A missing fixture fails here instead of skipping.
const lua = readFileSync(here(fixtures + "snapshot.lua"))
const expected = JSON.parse(readFileSync(here(fixtures + "snapshot.json"), "utf8"))
assert.deepStrictEqual(JSON.parse(parse(lua)), expected)

const outdated = new TextEncoder().encode(
  'ForeverCollectDB = {\n["schemaVersion"] = 8,\n["catalogs"] = { ["k"] = {} },\n}'
)
assert.throws(() => parse(outdated), Error)

console.log("golden fixture: ok")
