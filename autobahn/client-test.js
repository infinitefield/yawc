import { $ } from "https://deno.land/x/dax/mod.ts";
import { sleep } from "https://deno.land/x/sleep/mod.ts";

const pwd = new URL(".", import.meta.url).pathname;

const AUTOBAHN_TESTSUITE_DOCKER =
  "crossbario/autobahn-testsuite:0.8.2@sha256:5d4ba3aa7d6ab2fdbf6606f3f4ecbe4b66f205ce1cbc176d6cdf650157e52242";

const server =
  $`docker run --name fuzzingserver	-v ${pwd}/fuzzingserver.json:/fuzzingserver.json:ro	\
  -v ${pwd}/reports:/reports -p 9001:9001	--rm ${AUTOBAHN_TESTSUITE_DOCKER} \
  wstest -m fuzzingserver -s fuzzingserver.json`.spawn();
// sleep long because it might take a while to pull the files
await sleep(30);
await $`target/release/examples/autobahn_client`;

const { yawc } = JSON.parse(
  Deno.readTextFileSync("./autobahn/reports/client/index.json"),
);
const result = Object.values(yawc);

function failed(name) {
  return name != "OK" && name != "INFORMATIONAL" && name != "NON-STRICT";
}

const failedtests = result.filter((outcome) => failed(outcome.behavior));

console.log(
  `%c${result.length - failedtests.length} / ${result.length} tests OK`,
  `color: ${failedtests.length == 0 ? "green" : "red"}`,
);

Deno.exit(failedtests.length == 0 ? 0 : 1);
