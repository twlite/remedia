import { copyFileSync, mkdirSync } from 'node:fs';

const source = new URL('../../../scripts/', import.meta.url);
const destination = new URL('../public/', import.meta.url);

mkdirSync(destination, { recursive: true });

for (const file of ['install.sh', 'install.ps1']) {
  copyFileSync(new URL(file, source), new URL(file, destination));
}
