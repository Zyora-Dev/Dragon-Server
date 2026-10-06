import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const root = process.env.NIRAL_ROOT;
const port = Number(process.env.NIRAL_PORT);
if (!root || !Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error('NIRAL_ROOT and NIRAL_PORT (1..65535) are required');
}
const { checkRequiredEnv } = await import(pathToFileURL(resolve(root, 'src/server/hooks.js')));
const { missing } = await checkRequiredEnv(process.cwd());
if (missing.length) {
  throw new Error(`Missing required Niral environment variables: ${missing.join(', ')}`);
}
const { createProdServer } = await import(pathToFileURL(resolve(root, 'src/server/prod.js')));
const app = createProdServer({ dist: 'dist', cwd: process.cwd() });
app.server.listen(port, '127.0.0.1', () => {
  console.log(`Niral listening on 127.0.0.1:${port}`);
});
let stopping = false;
async function stop() {
  if (stopping) return;
  stopping = true;
  await app.shutdown({ grace: 750 });
  process.exit(0);
}
process.once('SIGTERM', stop);
process.once('SIGINT', stop);