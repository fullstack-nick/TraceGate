import http from "k6/http";
import { check, sleep } from "k6";
import { Rate } from "k6/metrics";

const expectedStatus = new Rate("expected_status");
const requestIdPresent = new Rate("request_id_present");

const baseUrl = (__ENV.BASE_URL || "https://127.0.0.1:8080").replace(/\/$/, "");
const apiKey = __ENV.API_KEY || "tracegate-demo-key";
const stressDuration = __ENV.STRESS_DURATION || "1h";
const spikeDuration = __ENV.SPIKE_DURATION || "5m";
const spikeVus = Number(__ENV.SPIKE_VUS || "80");
const soakVus = Number(__ENV.SOAK_VUS || "40");

export const options = {
  insecureSkipTLSVerify: true,
  scenarios: {
    feature_spikes: {
      executor: "ramping-vus",
      startVUs: 0,
      stages: [
        { duration: "30s", target: spikeVus },
        { duration: spikeDuration, target: spikeVus },
        { duration: "30s", target: 0 },
      ],
      exec: "mixedFeatureTraffic",
    },
    mixed_soak: {
      executor: "constant-vus",
      vus: soakVus,
      duration: stressDuration,
      startTime: spikeDuration,
      exec: "mixedFeatureTraffic",
    },
  },
  thresholds: {
    expected_status: ["rate>0.99"],
    request_id_present: ["rate>0.99"],
    http_req_duration: ["p(95)<1500", "p(99)<5000"],
  },
};

function record(response, expected, label) {
  expectedStatus.add(response.status === expected, { feature: label });
  requestIdPresent.add(Boolean(response.headers["X-Request-Id"]), { feature: label });
  check(response, {
    [`${label} returned ${expected}`]: (r) => r.status === expected,
    [`${label} returned x-request-id`]: (r) => Boolean(r.headers["X-Request-Id"]),
  });
}

function get(path, expected, label, headers = {}) {
  const response = http.get(`${baseUrl}${path}`, {
    headers,
    tags: { feature: label },
  });
  record(response, expected, label);
}

function post(path, body, expected, label, headers = {}) {
  const response = http.post(`${baseUrl}${path}`, body, {
    headers: { "content-type": "application/json", ...headers },
    tags: { feature: label },
  });
  record(response, expected, label);
}

export function mixedFeatureTraffic() {
  const choice = Math.random();
  const seq = `${__VU}-${__ITER}`;

  if (choice < 0.22) {
    get(`/api/users/${seq}`, 200, "routing_users");
  } else if (choice < 0.38) {
    get("/api/payments/fail", 403, "plugin_deny");
  } else if (choice < 0.54) {
    get("/api/plugin-timeout/proof", 403, "plugin_timeout");
  } else if (choice < 0.72) {
    get("/api/payments/fail", 500, "capture_failure", { "x-api-key": apiKey });
  } else if (choice < 0.86) {
    get(`/api/payments/slow?visible=yes&seq=${seq}`, 200, "slow_capture", { "x-api-key": apiKey });
  } else {
    post(
      `/api/payments/large-fail?visible=yes&seq=${seq}`,
      JSON.stringify({ note: "v1 stress capture proof", seq }),
      500,
      "large_capture",
      { "x-api-key": apiKey, "x-remove-me": "remove-this" },
    );
  }

  sleep(Math.random() * 0.2);
}
