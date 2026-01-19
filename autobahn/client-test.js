import { $ } from "https://deno.land/x/dax/mod.ts";
import { sleep } from "https://deno.land/x/sleep/mod.ts";

const pwd = new URL(".", import.meta.url).pathname;

const AUTOBAHN_TESTSUITE_DOCKER =
  "crossbario/autobahn-testsuite:25.10.1@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074";

const CONTAINER_NAME = "fuzzingserver";

async function containerExists(name) {
  const result = await $`docker ps -a --filter name=^/${name}$ --format "{{.Names}}"`.quiet();
  return result.stdout.trim().length > 0;
}

async function containerRunning(name) {
  const result = await $`docker ps --filter name=^/${name}$ --format "{{.Names}}"`.quiet();
  return result.stdout.trim().length > 0;
}

// Start
if (await containerExists(CONTAINER_NAME)) {
  if (await containerRunning(CONTAINER_NAME)) {
    console.log(`Autobahn ${CONTAINER_NAME} docker container already running: skipping startup.`);
  } else {
    console.log(`Autobahn ${CONTAINER_NAME} docker container exists but is stopped. Starting it.`);
    await $`docker start ${CONTAINER_NAME}`;
  }
} else {
  console.log(`Starting Autobahn ${CONTAINER_NAME} docker container...`);
  $`docker run --name ${CONTAINER_NAME} \
    -v ${pwd}/fuzzingserver.json:/fuzzingserver.json:ro \
    -v ${pwd}/reports:/reports \
    -p 9001:9001 \
    --rm ${AUTOBAHN_TESTSUITE_DOCKER} \
    wstest -m fuzzingserver -s fuzzingserver.json`.spawn();

   // sleep long because it might take a while to pull the files
   await sleep(30);
}

await $`cargo run --release --example autobahn_client`.env("RUST_BACKTRACE", "full");

const { yawc } = JSON.parse(
  Deno.readTextFileSync("./autobahn/reports/client/index.json"),
);

const result = Object.values(yawc);

function failed(name) {
  return name !== "OK" && name !== "INFORMATIONAL" && name !== "NON-STRICT";
}

const failedtests = result.filter((outcome) =>
  failed(outcome.behavior)
);

console.log(
  `%c${result.length - failedtests.length} / ${result.length} tests OK`,
  `color: ${failedtests.length === 0 ? "green" : "red"}`,
);

Deno.exit(failedtests.length === 0 ? 0 : 1);
