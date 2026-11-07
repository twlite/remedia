// These are FFmpeg's shared-library ABI majors, not its application version.
// Shared by the build-time verifier and the compiled artifact verifier.
export const ffmpegAbi = new Map([
  ['libavcodec', '63'],
  ['libavformat', '63'],
  ['libavutil', '61'],
  ['libavfilter', '12'],
  ['libswresample', '7'],
]);
