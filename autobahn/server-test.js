import { $ } from "https://deno.land/x/dax/mod.ts";
import { sleep } from "https://deno.land/x/sleep/mod.ts";

const AUTOBAHN_TESTSUITE_DOCKER =
  "crossbario/autobahn-testsuite:25.10.1@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074";

const pwd = new URL(".", import.meta.url).pathname;
const CONTAINER_NAME = "fuzzingclient";
const ECHO_SERVER_EXE = "target/release/examples/echo_server";

const isMac = Deno.build.os === "darwin";
const dockerHost = isMac ? "host.docker.internal" : "localhost";
const networkArgs = isMac ? "" : "--net=host";

async function containerExists(name) {
  const r = await $`docker ps -a --filter name=^/${name}$ --format "{{.Names}}"`.quiet();
  return r.stdout.trim().length > 0;
}

async function containerRunning(name) {
  const r = await $`docker ps --filter name=^/${name}$ --format "{{.Names}}"`.quiet();
  return r.stdout.trim().length > 0;
}

async function ensureEchoServerBuilt() {
  try {
    await Deno.stat(ECHO_SERVER_EXE);
  } catch {
    console.log("echo_server not found, building...");
    await $`cargo build --release --example echo_server`;
  }
}

// Start

const configPath = `${pwd}/fuzzingclient.json`;
const config = JSON.parse(Deno.readTextFileSync(configPath));
config.servers[0].url = `ws://${dockerHost}:9002`;
Deno.writeTextFileSync(configPath, JSON.stringify(config, null, 2));

await ensureEchoServerBuilt();

const controller = new AbortController();
const server = new Deno.Command(ECHO_SERVER_EXE, {
  signal: controller.signal,
}).spawn();

// Give server time to start
await sleep(5);

if (await containerExists(CONTAINER_NAME)) {
  if (await containerRunning(CONTAINER_NAME)) {
    console.log(`Autobahn ${CONTAINER_NAME} fuzzing client container already running: skipping.`);
  } else {
    console.log(`Autobahn ${CONTAINER_NAME} fuzzing client container exists, starting it.`);
    await $`docker start ${CONTAINER_NAME}`;
  }
} else {
  console.log(`Starting Autobahn ${CONTAINER_NAME} fuzzing client container...`);
  const cmd = [
    "docker run",
    `--name ${CONTAINER_NAME}`,
    `-v ${pwd}/fuzzingclient.json:/fuzzingclient.json:ro`,
    `-v ${pwd}/reports:/reports`,
    "-p 9002:9002",
    networkArgs,
    "--rm",
    AUTOBAHN_TESTSUITE_DOCKER,
    "wstest -m fuzzingclient -s fuzzingclient.json",
  ]
    .filter(Boolean)
    .join(" ");

  await $.raw(cmd).cwd(pwd);
}

controller.abort();

const indexJson = JSON.parse(
  Deno.readTextFileSync("./autobahn/reports/servers/index.json"),
);
const testResults = Object.values(indexJson.yawc);

function isFailure(behavior) {
  return !["OK", "INFORMATIONAL", "NON-STRICT"].includes(behavior);
}

const failedTests = testResults.filter((o) =>
  isFailure(o.behavior)
);
const passedTests = testResults.length - failedTests.length;

console.log(
  `%c${passedTests} / ${testResults.length} tests OK`,
  `color: ${failedTests.length === 0 ? "green" : "red"}`,
);

Deno.exit(failedTests.length === 0 ? 0 : 1);
