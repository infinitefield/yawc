import { $ } from "https://deno.land/x/dax/mod.ts";
import { sleep } from "https://deno.land/x/sleep/mod.ts";

const pwd = new URL(".", import.meta.url).pathname;

const AUTOBAHN_TESTSUITE_DOCKER =
  "crossbario/autobahn-testsuite:25.10.1@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074";

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
