import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

// ffmpeg-sys-next 9.0.1 uses a Vulkan ABI shim during bindgen. Its hard-coded
// 240-byte assertion is correct for 64-bit pointers, but not 32-bit targets.
// Only change the assertion, preserving the struct and bindgen's target layout.
// Remove this workaround when an upstream release fixes the assertion.
const cargoHome = process.env.CARGO_HOME ?? join(homedir(), '.cargo');
const registry = join(cargoHome, 'registry', 'src');
const crate = 'ffmpeg-sys-next-9.0.1';
const headerName = join('hwcontext_stubs', 'vulkan', 'vulkan.h');
const original = '_Static_assert(sizeof(VkPhysicalDeviceFeatures2) == 240,';
const corrected =
  '_Static_assert(sizeof(VkPhysicalDeviceFeatures2) == (sizeof(void *) == 8 ? 240 : 228),';

if (!existsSync(registry)) {
  throw new Error(`Cargo registry sources are unavailable: ${registry}`);
}

const matches = readdirSync(registry)
  .map((root) => join(registry, root, crate, headerName))
  .filter((path) => existsSync(path));

if (matches.length !== 1) {
  throw new Error(`Expected one unpacked ${crate} Vulkan shim, found ${matches.length}`);
}

const path = matches[0];
const content = readFileSync(path, 'utf8');
if (!content.includes(original)) {
  throw new Error(`The expected ${crate} Vulkan ABI assertion changed; inspect upstream before patching`);
}

writeFileSync(path, content.replace(original, corrected));
console.log(`Adjusted ${crate} Vulkan ABI assertion for 32-bit bindgen`);
