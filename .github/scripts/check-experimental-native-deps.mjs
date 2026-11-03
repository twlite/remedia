const [target, blocker] = process.argv.slice(2);
if (!target || !blocker) {
  throw new Error('Usage: check-experimental-native-deps.mjs <target> <known-blocker>');
}

console.log('::warning title=Experimental native target::' + target + ': ' + blocker);
console.log(
  'The workflow will still attempt the target compile. A Rust target and cross-linker do not replace target-compatible FFmpeg 7 headers, libraries, and pkg-config metadata.',
);
