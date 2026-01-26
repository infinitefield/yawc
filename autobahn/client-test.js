import { $ } from "https://deno.land/x/dax/mod.ts";
import { sleep } from "https://deno.land/x/sleep/mod.ts";

const pwd = new URL(".", import.meta.url).pathname;

const AUTOBAHN_TESTSUITE_DOCKER =
  "crossbario/autobahn-testsuite:25.10.1@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074";

// Accept optional feature flag from command line (e.g., "zlib")
const FEATURE_FLAG = Deno.args[0] || "";
const FEATURE_SUFFIX = FEATURE_FLAG ? `_${FEATURE_FLAG}` : "";
const CONTAINER_NAME = `fuzzingserver${FEATURE_SUFFIX}`;
const PORT = FEATURE_FLAG === "zlib" ? 9002 : 9001;
const CLIENT_EXE = "target/release/examples/autobahn_client";

async function containerExists(name) {
  const result =
    await $`docker ps -a --filter name=^/${name}$ --format "{{.Names}}"`.quiet();
  return result.stdout.trim().length > 0;
}

async function containerRunning(name) {
  const result =
    await $`docker ps --filter name=^/${name}$ --format "{{.Names}}"`.quiet();
  return result.stdout.trim().length > 0;
}

async function ensureClientBuilt() {
  console.log(
    `Building autobahn_client${FEATURE_FLAG ? ` with feature: ${FEATURE_FLAG}` : ""}...`,
  );
  if (FEATURE_FLAG) {
    await $`cargo build --release --example autobahn_client --features ${FEATURE_FLAG}`;
  } else {
    await $`cargo build --release --example autobahn_client`;
  }
}

// Start
if (await containerExists(CONTAINER_NAME)) {
  if (await containerRunning(CONTAINER_NAME)) {
    console.log(
      `Autobahn ${CONTAINER_NAME} docker container already running: skipping startup.`,
    );
  } else {
    console.log(
      `Autobahn ${CONTAINER_NAME} docker container exists but is stopped. Starting it.`,
    );
    await $`docker start ${CONTAINER_NAME}`;
  }
} else {
  console.log(
    `Starting Autobahn ${CONTAINER_NAME} docker container on port ${PORT}...`,
  );
  $`docker run --name ${CONTAINER_NAME} \
    -v ${pwd}/fuzzingserver.json:/fuzzingserver.json:ro \
    -v ${pwd}/reports:/reports \
    -p ${PORT}:9001 \
    --rm ${AUTOBAHN_TESTSUITE_DOCKER} \
    wstest -m fuzzingserver -s fuzzingserver.json`.spawn();
}

await ensureClientBuilt();
await $`${CLIENT_EXE}`
  .env("RUST_BACKTRACE", "full")
  .env("AUTOBAHN_PORT", PORT.toString());

const { yawc } = JSON.parse(
  Deno.readTextFileSync("./autobahn/reports/client/index.json"),
);

const result = Object.values(yawc);

function failed(name) {
  return name !== "OK" && name !== "INFORMATIONAL" && name !== "NON-STRICT";
}

const failedtests = result.filter((outcome) => failed(outcome.behavior));

console.log(
  `%c${result.length - failedtests.length} / ${result.length} tests OK`,
  `color: ${failedtests.length === 0 ? "green" : "red"}`,
);

Deno.exit(failedtests.length === 0 ? 0 : 1);
