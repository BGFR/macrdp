# Tutorial: forward the macrdp audit stream to a SIEM (OpenSearch)

A complete, copy-paste walkthrough that stands up a real open-source SIEM on your Mac and
detects an RDP brute-force against macrdp — end to end, in about 15 minutes.

It uses **[OpenSearch]** + **OpenSearch Dashboards** (the Apache-2.0 fork of Elasticsearch /
Kibana). OpenSearch ships a bundled **Security Analytics** plugin — a genuine SIEM with
Sigma-based detection rules, findings, and alerts — so this is "forward to a SIEM," not just
"ship logs to a search box." Everything here is free, self-hosted, and runs on one laptop.

```
 macrdp  ──(--audit-file)──▶  ~/Library/Logs/macrdp-audit.log
                                          │
                                     Fluent Bit (tail)          ← the collector
                                          │  HTTPS
                                          ▼
                                   OpenSearch  :9200            ← store + Security Analytics (the SIEM)
                                          │
                                          ▼
                           OpenSearch Dashboards  :5601         ← search, detectors, findings
```

This is the pipeline from [`siem-forwarding.md`](siem-forwarding.md) made concrete. For what
each event and field **means**, keep [`audit-log.md`](audit-log.md) open alongside.

> **Demo posture, not production.** This tutorial uses a single OpenSearch node, self-signed
> TLS, and a throwaway admin password so it runs on one machine with zero external setup. The
> [Hardening for production](#hardening-for-production) section at the end says exactly what to
> change before this touches real traffic. **Don't run the demo config as-is on anything but a
> lab box.**

## Prerequisites

- **Docker Desktop** (for OpenSearch + Dashboards). ~2 GB free RAM for the stack.
- **Fluent Bit** — the log collector: `brew install fluent-bit`.
- **macrdp** — a build with `--audit-file` (v0.8.33+): `cargo build --release`.
- **sdl-freerdp** (to generate test connections): `brew install freerdp`.
- macOS. Replace `YOU` with your username (`whoami`) wherever it appears below.

---

## Step 1 — turn on macrdp's JSON audit stream

Run macrdp on **loopback** with the audit stream pointed at a stable path. Loopback is exempt
from the auth *guard* (so a wrong-password loop can't lock you out mid-tutorial), but it still
emits the `accept` / `auth` / `disconnect` audit events — which is exactly what we want to
forward. `--skip-auth` still makes CredSSP validate against the throwaway `--username` /
`--password`, so a wrong password produces a real `auth` failure.

```bash
mkdir -p ~/Library/Logs
./target/release/macrdp \
  --bind 127.0.0.1:3390 --skip-auth --allow-sleep \
  --username audittest --password 's3cret-good' \
  --audit-file ~/Library/Logs/macrdp-audit.log
```

Leave it running in this terminal. In a second terminal, prove the file gets written:

```bash
sdl-freerdp /v:127.0.0.1:3390 /u:audittest /p:'s3cret-good' /cert:ignore +auth-only /log-level:ERROR
tail -n 3 ~/Library/Logs/macrdp-audit.log
```

You should see JSON lines like:

```json
{"timestamp":"2026-07-10T18:22:04.117Z","level":"INFO","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.33","host":"your-mac","event":"accept","src_ip":"127.0.0.1","src_port":54132}
{"timestamp":"2026-07-10T18:22:04.402Z","level":"INFO","target":"macrdp::audit","schema_version":1,"macrdp_version":"0.8.33","host":"your-mac","event":"auth","src_ip":"127.0.0.1","src_port":54132,"outcome":"success"}
```

> `+auth-only` runs just the CredSSP exchange and exits — no window. It makes two TCP
> connections per invocation (a pre-NLA probe + the NLA connection), so you'll see an extra
> `accept`/`disconnect` pair with no `auth` event. That's normal; ignore it.

---

## Step 2 — stand up OpenSearch + Dashboards

Create a working directory and drop in a compose file:

```bash
mkdir -p ~/macrdp-siem && cd ~/macrdp-siem
```

`~/macrdp-siem/docker-compose.yml`:

```yaml
services:
  opensearch:
    image: opensearchproject/opensearch:2.17.1
    container_name: macrdp-opensearch
    environment:
      - discovery.type=single-node
      - bootstrap.memory_lock=true
      - "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m"
      # Demo-only credential. Must satisfy OpenSearch's strong-password policy.
      - OPENSEARCH_INITIAL_ADMIN_PASSWORD=MacrdpSiem!2026
    ulimits:
      memlock: { soft: -1, hard: -1 }
      nofile:  { soft: 65536, hard: 65536 }
    ports:
      - "9200:9200"
    networks: [siem]

  dashboards:
    image: opensearchproject/opensearch-dashboards:2.17.1
    container_name: macrdp-dashboards
    environment:
      - 'OPENSEARCH_HOSTS=["https://opensearch:9200"]'
    ports:
      - "5601:5601"
    depends_on: [opensearch]
    networks: [siem]

networks:
  siem:
```

Bring it up:

```bash
docker compose up -d
```

OpenSearch takes ~60–90 s to go green on first run. Verify (self-signed TLS, hence `-k`):

```bash
curl -sk -u admin:'MacrdpSiem!2026' https://localhost:9200/_cluster/health | python3 -m json.tool
```

Wait for `"status": "green"` or `"yellow"` (yellow is fine for a single node). Dashboards will
be at **http://localhost:5601** shortly after — log in with `admin` / `MacrdpSiem!2026`.

---

## Step 3 — ship events with Fluent Bit

Fluent Bit tails the native log file and writes each JSON line as one OpenSearch document. Run
it **natively** (via brew) rather than in a container — a native tail on a native file has
reliable file-change notification, whereas tailing a bind-mounted macOS file from inside a
container is flaky.

`~/macrdp-siem/parsers.conf`:

```ini
[PARSER]
    Name        macrdp_json
    Format      json
    Time_Key    timestamp
    Time_Format %Y-%m-%dT%H:%M:%S.%L%z
```

`~/macrdp-siem/fluent-bit.conf` (replace `YOU`):

```ini
[SERVICE]
    Flush        1
    Log_Level    info
    Parsers_File parsers.conf

[INPUT]
    Name             tail
    Path             /Users/YOU/Library/Logs/macrdp-audit.log
    Tag              macrdp.audit
    Parser           macrdp_json
    Read_from_Head   true
    Refresh_Interval 5

[OUTPUT]
    Name               opensearch
    Match              macrdp.audit
    Host               localhost
    Port               9200
    HTTP_User          admin
    HTTP_Passwd        MacrdpSiem!2026
    TLS                On
    TLS.Verify         Off
    Index              macrdp-audit
    Suppress_Type_Name On
```

Run it (leave it in its own terminal):

```bash
cd ~/macrdp-siem && fluent-bit -c fluent-bit.conf
```

Confirm the documents landed:

```bash
curl -sk -u admin:'MacrdpSiem!2026' 'https://localhost:9200/macrdp-audit/_count?pretty'
```

`count` should be non-zero and climb each time you run another `sdl-freerdp` connection.

> **`Suppress_Type_Name On`** is required — OpenSearch (like ES 7+) removed mapping types and
> rejects the legacy `_doc` type otherwise. If the `Time_Format` line ever logs a parse warning,
> it's non-fatal: the doc still ingests, just stamped at collection time instead of event time.

---

## Step 4 — see the events in Dashboards

1. Open **http://localhost:5601** → log in (`admin` / `MacrdpSiem!2026`).
2. Left menu → **Dashboards Management → Index patterns → Create index pattern**.
3. Pattern: `macrdp-audit*` → **Next** → Time field: **`@timestamp`** → **Create**.
   (Fluent Bit's OpenSearch output writes the event time to `@timestamp`; the original
   `timestamp` string stays on the document too.)
4. Left menu → **Discover**, pick the `macrdp-audit*` pattern. Every connection you make with
   `sdl-freerdp` now appears here. Try a search bar filter: `event:auth and outcome:did_not_complete`.

![OpenSearch Dashboards Discover showing macrdp audit events — each row is one JSON audit event with level, schema_version, event, src_ip, src_port, outcome and reason fields, filtered to event:auth](images/siem-discover.png)

You now have searchable RDP auth telemetry. The next step turns it into a **detection**.

---

## Step 5 — a real SIEM detection (Security Analytics)

We'll alert on **repeated CredSSP auth failures from one source IP** — the RDP brute-force
signature. This uses the bundled Security Analytics plugin: a **custom log type** (so it knows
about macrdp events), a **Sigma rule**, and a **detector** that runs the rule on a schedule.

### 5a. Create a custom log type

Left menu → **Security Analytics → Log types → Create log type** (in older builds this lives under the
**Detectors** area).
- Name: `macrdp`
- Category: **Other** (or Access Management) → **Create**.

### 5b. Create the detection rule

**Security Analytics → Rules → Create detection rule**, switch to the **YAML editor**, and
paste (generate your own UUID with `uuidgen`):

```yaml
title: macrdp repeated CredSSP auth failures (possible RDP brute force)
id: 8f9e2c14-6b3a-4d5e-9f1a-2c7b8e0d4a11
status: experimental
description: More than 5 CredSSP auth failures from one source IP within the detector window.
author: you
date: 2026/07/10
logsource:
  product: macrdp
detection:
  sel:
    event: auth
    outcome: did_not_complete
  condition: sel | count(*) by src_ip > 5
level: high
```

Set the rule's **log type** to `macrdp` and save. This is an **aggregation rule** — it fires
when a single `src_ip` produces **more than 5** `auth did_not_complete` events within the
detector's evaluation window (set by the detector schedule in the next step).

Two things that will bite you if you skip them (both confirmed the hard way):
- **`id:` must be a valid UUID.** Security Analytics rejects a rule without one
  (`Sigma rule identifier must be an UUID`). Run `uuidgen` and paste the result — don't hand-type
  a UUID-shaped string.
- **Use `count(*)`, not `count()`.** OpenSearch's aggregation backend requires the `*`; a plain
  Sigma `count()` fails on import.

> Want a finding on *every* failure while you learn (no aggregation)? Use `condition: sel` (drop
> the `| count(*) …` pipe). Switch back to the aggregated version for the realistic brute-force
> detection.

Once saved, your custom rule sits at the top of the Detection rules library alongside the
bundled Sigma rules:

![Security Analytics Detection rules list with the custom "macrdp repeated CredSSP auth failures" rule at the top — High severity, log type Other: Macrdp, source Custom — above the bundled standard rules](images/siem-rules.png)

### 5c. Create the detector

**Security Analytics → Detectors → Create detector**.
- **Data source**: index `macrdp-audit`.
- **Log type**: `macrdp`.
- **Detection rules**: enable the `macrdp repeated CredSSP auth failures` rule you just made
  (disable the noise of unrelated built-in rules).
- **Field mapping**: map the rule fields (`event`, `outcome`, `src_ip`) to the same-named index
  fields. Because the names match, the auto-suggested mapping is usually correct — accept it (or
  map each field to itself if prompted).
- **Detector schedule**: every **1 minute** (so the tutorial fires quickly).
- **Alert trigger**: add one — name it `macrdp brute force`, and set its condition to match on
  **rule severity `high`** (not the specific rule), alert severity `1 (Highest)`. Keying on
  *severity* matters: an aggregation rule's findings are attributed to an auto-generated
  "chained" rule, so a trigger keyed on your rule's name/id won't match them — severity does. A
  notification channel (email/Slack/webhook) is optional; the **alert is recorded in
  Dashboards** regardless.
- **Create**.

The created detector shows its full configuration — data source, schedule, log type, and the
associated rule with its severity:

![The macrdp-brute-force detector detail page — Active, schedule "Every 1 minute", data source macrdp-audit, log type Macrdp, and the associated "macrdp repeated CredSSP auth failures" rule at High severity](images/siem-detector.png)

---

## Step 6 — trigger the detection

> **Order matters:** create the detector (Step 5c) *first*, then generate this traffic. Like an
> alerting monitor, the detector only evaluates events that arrive **after** it starts — a burst
> you ran before the detector existed won't produce a finding.

With macrdp (Step 1) and Fluent Bit (Step 3) still running, fire a burst of **wrong-password**
connections from a third terminal:

```bash
for i in $(seq 1 8); do
  sdl-freerdp /v:127.0.0.1:3390 /u:audittest /p:'WRONG-PASSWORD' \
    /cert:ignore +auth-only /log-level:ERROR
done
```

Each is a real CredSSP failure → an `auth`/`did_not_complete` event → shipped by Fluent Bit →
matched by the detector on its next 1-minute run.

Watch it land (typically within ~1 minute of the detector's next run):
- **Security Analytics → Alerts** — the authoritative signal: the `brute-force` alert is
  **Active**, severity **1 (Highest)**, detector `macrdp-brute-force`. This is what proves the
  detection fired.
- **Discover**: `event:auth and outcome:did_not_complete` shows the underlying failures.
- **Security Analytics → Findings** — heads-up: for an *aggregation* rule the Findings tab
  often renders **empty even though findings exist and the alert fired** (the bucket-level
  findings carry no detector id for the tab to group on — the alert is the reliable UI signal).
  The non-aggregation variant (`condition: sel` from Step 5b) *does* populate the Findings tab
  if you want to see findings there.

![Security Analytics Alerts page — the histogram shows 24 ACTIVE alerts and the table lists rows with trigger name "brute-force", detector macrdp-brute-force, status Active, severity 1 (Highest)](images/siem-alerts.png)

That's the full loop: an RDP attack against macrdp surfaced as a SIEM alert. Correlate the
attacker by `src_ip` (Discover → filter on the finding's IP) to see its `accept` attempts and
that **no `auth` `success` ever occurred** — the signature of a failed brute-force.

### Troubleshooting

- **`_count` stays 0 in Step 3** — check the Fluent Bit `Path` (did you replace `YOU`?), that
  macrdp is actually writing the file (`tail -f ~/Library/Logs/macrdp-audit.log`), and the
  Fluent Bit terminal for an `[output:opensearch]` connection error (usually the admin password
  or `TLS.Verify Off` missing).
- **No findings** — the detector only sees events that arrive **after** it's created (generate
  the burst *after* Step 5c), and the aggregation needs **more than 5** failures from one IP
  within the window; run the 8-connection burst in one go.
- **Findings but no alert** — the trigger must match on **rule severity `high`**, not the rule
  name/id (aggregation findings are attributed to a chained rule). Re-check Step 5c's trigger.
- **Aggregation never counts** — the `by src_ip` grouping needs an aggregatable field. The
  default dynamic mapping gives you `src_ip` (text) *and* `src_ip.keyword`; make sure the
  detector's field mapping resolves `src_ip` to the keyword field (the auto-mapping normally
  does). You can confirm the underlying logic independently with a direct query:
  ```bash
  curl -sk -u admin:'MacrdpSiem!2026' 'https://localhost:9200/macrdp-audit/_search' \
    -H 'Content-Type: application/json' -d '{"size":0,
      "query":{"bool":{"filter":[{"term":{"event.keyword":"auth"}},{"term":{"outcome.keyword":"did_not_complete"}}]}},
      "aggs":{"by_ip":{"terms":{"field":"src_ip.keyword","min_doc_count":6}}}}'
  ```
  A non-empty `by_ip` bucket is an IP the rule would fire on.

---

## Cleanup

```bash
# Stop Fluent Bit (Ctrl-C in its terminal) and macrdp (Ctrl-C in its terminal), then:
cd ~/macrdp-siem && docker compose down -v      # -v also drops the OpenSearch data volume
```

---

## Hardening for production

The demo cuts every corner that a lab tolerates and production doesn't. Before this pipeline
touches real traffic:

- **Real TLS, verified.** Replace the self-signed demo certs and set Fluent Bit `TLS.Verify On`
  with the CA bundle. Never ship `TLS.Verify Off` off a lab box.
- **Real credentials.** Change the OpenSearch admin password, create a **least-privilege
  ingest user** for Fluent Bit (write-only to `macrdp-audit*`), and keep secrets out of the
  config file (Fluent Bit env vars / a secrets store).
- **Bind macrdp off-loopback for real telemetry.** Loopback is guard-exempt, so you only get
  `reject` / `lockout` events (and real threat traffic) once `--bind` exposes the server to the
  network. See the guard knobs in [`configuration.md`](configuration.md). And never put an RDP
  server on a raw public IP — reach it over a VPN / RD Gateway.
- **Right-size the store.** A single OpenSearch node with 512 MB heap is a demo. Real
  deployments want a multi-node cluster (or a managed OpenSearch), index lifecycle management to
  age out old audit indices, and snapshots.
- **Run the collector as a service.** Turn Fluent Bit into a `launchd` agent so it survives
  reboots and starts before macrdp. Its file tail resumes from a stored offset, and macrdp's
  audit file self-rotates under a **stable path**, so the collector never needs reconfiguring.
- **Schema-version your rules.** Key detections off `schema_version` — it only bumps on a
  breaking field change, so a rule pinned to `schema_version: 1` won't silently mis-parse a
  future format.

## Adapting to a different SIEM

The macrdp side never changes — it's just a JSON file. Swap **Step 3's Fluent Bit output** (or
use Vector / rsyslog instead) for your SIEM's sink; [`siem-forwarding.md`](siem-forwarding.md)
has Splunk-HEC / Elasticsearch / syslog examples. Everything upstream of the collector is
identical.

## See also

- [`audit-log.md`](audit-log.md) — what each event and field means, with worked interpretation examples.
- [`siem-forwarding.md`](siem-forwarding.md) — the JSON schema (v1) + Vector / Fluent Bit / rsyslog collector configs.
- [`configuration.md`](configuration.md) — the `--audit-file` flag and the auth-guard (rate-limit / lockout) knobs.

[OpenSearch]: https://opensearch.org/
