import { $ } from "https://deno.land/x/dax/mod.ts";
import { sleep } from "https://deno.land/x/sleep/mod.ts";

// Configuration
const AUTOBAHN_TESTSUITE_DOCKER =
  "crossbario/autobahn-testsuite:25.10.1@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074";
const pwd = new URL(".", import.meta.url).pathname;

// Platform-specific settings
const isMac = Deno.build.os === "darwin";
const dockerHost = isMac ? "host.docker.internal" : "localhost";
const networkArgs = isMac ? "" : "--net=host";

// Update config to use correct host
const configPath = `${pwd}/fuzzingclient.json`;
const config = JSON.parse(Deno.readTextFileSync(configPath));
config.servers[0].url = `ws://${dockerHost}:9001`;

Deno.writeTextFileSync(configPath, JSON.stringify(config, null, 2));

// Start the WebSocket echo server
const controller = new AbortController();
const server = new Deno.Command("target/release/examples/echo_server", {
  signal: controller.signal,
}).spawn();

// Give server time to start
await sleep(5);

// Run Autobahn fuzzing test suite
const cmd = [
  "docker run",
  "--name fuzzingclient",
  `-v ${pwd}/fuzzingclient.json:/fuzzingclient.json:ro`,
  `-v ${pwd}/reports:/reports`,
  "-p 9001:9001",
  networkArgs,
  "--rm",
  AUTOBAHN_TESTSUITE_DOCKER,
  "wstest -m fuzzingclient -s fuzzingclient.json",
]
  .filter(Boolean)
  .join(" ");

await $.raw(cmd).cwd(pwd); // Cleanup
controller.abort();

// Parse test results
const indexJson = JSON.parse(
  Deno.readTextFileSync("./autobahn/reports/servers/index.json"),
);
const testResults = Object.values(indexJson.yawc);

// Check for failures
function isFailure(behavior) {
  const passing = ["OK", "INFORMATIONAL", "NON-STRICT"];
  return !passing.includes(behavior);
}

const failedTests = testResults.filter((outcome) =>
  isFailure(outcome.behavior),
);
const passedTests = testResults.length - failedTests.length;

// Display results
const color = failedTests.length === 0 ? "green" : "red";
console.log(
  `%c${passedTests} / ${testResults.length} tests OK`,
  `color: ${color}`,
);

// Exit with appropriate code
Deno.exit(failedTests.length === 0 ? 0 : 1);
