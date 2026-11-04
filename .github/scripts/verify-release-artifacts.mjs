import { verifyReleaseArtifacts } from './release.mjs';

const [bindingsDirectory, serversDirectory] = process.argv.slice(2);
if (!bindingsDirectory || !serversDirectory) {
  throw new Error(
    'Usage: verify-release-artifacts.mjs <bindings-directory> <server-binaries-directory>',
  );
}

const artifacts = verifyReleaseArtifacts(bindingsDirectory, serversDirectory);
for (const artifact of artifacts) {
  if (artifact.server) {
    console.log(`Verified ${artifact.triple}: ${artifact.addon} and ${artifact.server}`);
  } else {
    console.log(`Verified ${artifact.triple}: ${artifact.addon} (N-API addon only)`);
  }
}
