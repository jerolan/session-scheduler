const assert = require("node:assert/strict");
const { spawn, execFile } = require("node:child_process");
const { once } = require("node:events");
const { randomUUID, randomBytes, createHmac } = require("node:crypto");
const fs = require("node:fs/promises");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");

const [schedulerExe, openCodeExe] = process.argv.slice(2);
assert(schedulerExe && openCodeExe, "Usage: node tests/opencode_owner_windows.cjs scheduler.exe opencode.exe");
const timeout = 20_000;
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const deadline = async (probe, description) => {
    const expires = Date.now() + timeout;
    while (Date.now() < expires) {
        const result = await probe();
        if (result) return result;
        await pause(50);
    }
    throw new Error(description);
};
function listen(handler) {
    const server = http.createServer(handler);
    return new Promise(resolve => server.listen(0, "127.0.0.1", () => resolve(server)));
}

async function main() {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "ss12-21-c-"));
    const workspace = path.join(root, "workspace");
    await fs.mkdir(workspace);
    const otherWorkspace = path.join(root, "other");
    await fs.mkdir(otherWorkspace);
    const processEnv = Object.fromEntries(
        Object.entries(process.env).filter(([key]) =>
            ["systemroot", "windir", "comspec", "path", "pathext"].includes(key.toLowerCase())),
    );
    for (const name of ["HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP",
        "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"]) {
        processEnv[name] = path.join(root, name);
        await fs.mkdir(processEnv[name]);
    }
    const children = [];
    const state = path.join(root, "scheduler-state");
    const readOwner = async id => JSON.parse(await fs.readFile(path.join(state, "owner-" + id + ".json"), "utf8"));
    const ownerPids = new Set();
    function alive(pid) {
        try { process.kill(pid, 0); return true; }
        catch (error) { if (error.code === "ESRCH") return false; throw error; }
    }
    function start(exe, args, additions = {}) {
        const process = spawn(exe, args, {
            cwd: workspace, env: { ...processEnv, ...additions }, windowsHide: true,
        });
        children.push(process);
        process.stderr.setEncoding("utf8");
        process.stderr.on("data", chunk => { process.diagnostic = (process.diagnostic ?? "") + chunk; });
        return process;
    }
    async function stop(child) {
        if (child.exitCode !== null || child.signalCode !== null) return;
        const finished = new Promise(resolve => child.once("exit", resolve));
        child.kill();
        await finished;
        child.stdout.destroy();
        child.stderr.destroy();
    }
    const requests = [];
    const modelKey = "isolated-model-key";
    let releaseBusy, releaseCounterpart, releaseStreaming;
    const busy = new Promise(resolve => { releaseBusy = resolve; });
    const counterpart = new Promise(resolve => { releaseCounterpart = resolve; });
    const streaming = new Promise(resolve => { releaseStreaming = resolve; });
    const model = await listen(async (request, response) => {
        if (request.headers.authorization !== "Bearer " + modelKey) {
            response.writeHead(401).end();
            return;
        }
        const chunks = [];
        for await (const chunk of request) chunks.push(chunk);
        const body = JSON.parse(Buffer.concat(chunks));
        const text = body.messages.filter(message => message.role === "user")
            .flatMap(message => typeof message.content === "string"
                ? [message.content]
                : message.content.map(part => part.text)).join(" ");
        const title = text.includes("Generate a title for this conversation:");
        if (!title) requests.push(text);
        response.writeHead(200, { "Content-Type": "text/event-stream" });
        response.flushHeaders();
        if (!title && text.includes("assignment-counterpart")) await counterpart;
        if (!title && text.includes("assignment-hold") && !text.includes("assignment-next")) await busy;
        const frame = { id: "completions-test", object: "chat.completion.chunk", created: 1, model: "probe" };
        function sendDelta(delta, finish_reason) {
            response.write("data: " + JSON.stringify({
                ...frame, choices: [{ index: 0, delta, finish_reason }],
            }) + "\n\n");
        }
        if (!title && text.includes("assignment-stream")) {
            sendDelta({ role: "assistant", content: "owner-probe-" }, null);
            await streaming;
            sendDelta({ content: "pong" }, null);
        } else {
            sendDelta({ role: "assistant", content: "owner-probe-pong" }, null);
        }
        sendDelta({}, "stop");
        response.end("data: [DONE]\n\n");
    });
    const deliveries = [];
    const signingKey = randomBytes(32);
    const events = await listen(async (request, response) => {
        const chunks = [];
        for await (const chunk of request) chunks.push(chunk);
        const body = Buffer.concat(chunks);
        const signed = createHmac("sha256", signingKey)
            .update(request.headers["webhook-id"] + "." + request.headers["webhook-timestamp"] + ".")
            .update(body).digest("base64");
        const event = JSON.parse(body);
        if (request.headers["webhook-signature"] !== "v1," + signed) {
            response.writeHead(400).end();
            return;
        }
        if (event.type === "verification") {
            response.writeHead(200, { "Content-Type": "application/json" })
                .end(JSON.stringify({ challenge: event.challenge }));
        } else {
            deliveries.push(event);
            response.writeHead(200).end();
        }
    });
    try {
        const data = path.join(processEnv.XDG_DATA_HOME, "opencode");
        await fs.mkdir(data);
        await fs.writeFile(path.join(data, "auth.json"),
            JSON.stringify({ probe: { type: "api", key: modelKey } }));
        Object.assign(processEnv, {
            OPENCODE_DISABLE_AUTOUPDATE: "1",
            OPENCODE_DISABLE_DEFAULT_PLUGINS: "1",
            OPENCODE_DISABLE_MODELS_FETCH: "1",
            OPENCODE_CONFIG_CONTENT: JSON.stringify({
                model: "probe/probe", permission: { "*": "deny" },
                provider: { probe: {
                    npm: "@ai-sdk/openai-compatible", name: "Local acceptance",
                    options: { baseURL: "http://127.0.0.1:" + model.address().port + "/v1" },
                    models: { probe: { name: "Probe", limit: { context: 32000, output: 256 } } },
                } },
            }),
        });
        const portReservation = await listen();
        const port = portReservation.address().port;
        await new Promise(resolve => portReservation.close(resolve));
        const bearer = randomUUID();
        const callback = "http://127.0.0.1:" + events.address().port + "/callback";
        const environment = {
            SESSION_SCHEDULER_HTTP_PORT: port,
            SESSION_SCHEDULER_BEARER_TOKEN: bearer,
            SESSION_SCHEDULER_ACCOUNT: "isolated-owner-acceptance",
            SESSION_SCHEDULER_STATE_DIR: state,
            SESSION_SCHEDULER_CALLBACK_URL: callback,
            SESSION_SCHEDULER_CALLBACK_IP: "127.0.0.1",
            SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK: "1",
            SESSION_SCHEDULER_WORKSPACES: [workspace, otherWorkspace].join(path.delimiter),
            SESSION_SCHEDULER_OPENCODE_COMMAND: openCodeExe,
        };
        let scheduler = start(schedulerExe, [], environment);
        async function rpc(method, params) {
            const response = await fetch("http://127.0.0.1:" + port + "/mcp", {
                method: "POST",
                headers: { Authorization: "Bearer " + bearer, "Content-Type": "application/json", Connection: "close" },
                body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
                signal: AbortSignal.timeout(8000),
            });
            assert.equal(response.status, 200);
            const envelope = await response.json();
            assert(!envelope.error, JSON.stringify(envelope.error));
            return envelope.result;
        }
        async function ready() {
            await deadline(async () => {
                if (scheduler.exitCode !== null) throw new Error(scheduler.diagnostic);
                try {
                    return await rpc("initialize", {
                        protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "owner-test", version: "1" },
                    });
                } catch (error) {
                    if (error.name === "TimeoutError") throw new Error("initialize timed out: " + scheduler.diagnostic);
                    if (!["ECONNREFUSED", "ECONNRESET", "UND_ERR_SOCKET"].includes(error.cause?.code)) throw error;
                }
            }, "Scheduler was unavailable");
        }
        async function call(name, args) {
            let result;
            try { result = await rpc("tools/call", { name, arguments: args }); }
            catch (error) {
                console.error("MCP call failed:", name, scheduler.diagnostic);
                throw error;
            }
            const content = result.content[0].text;
            return { error: result.isError, value: JSON.parse(content) };
        }
        const inspect = async id => (await call("inspect_session", { session_id: id })).value;
        const delegate = prompt => call("delegate", { workspace, agent: "opencode", prompt });
        const follow = (id, prompt) => call("follow_up", { session_id: id, prompt });
        const reconcile = (id, turn) => call("reconcile_session", { session_id: id, turn_id: turn });
        await ready();
        const external = start(openCodeExe, ["acp", "--hostname", "127.0.0.1", "--port", "0", "--no-mdns"],
            { OPENCODE_SERVER_PASSWORD: randomUUID() });
        const replies = require("node:readline").createInterface({ input: external.stdout });
        async function externalRequest(id, method, params) {
            return new Promise((resolve, reject) => {
                const timer = setTimeout(() => {
                    replies.off("line", receive);
                    reject(new Error("External OpenCode timed out at " + method));
                }, 15000);
                function receive(line) {
                    const reply = JSON.parse(line);
                    if (reply.id !== id || reply.method) return;
                    clearTimeout(timer);
                    replies.off("line", receive);
                    if (reply.error) reject(new Error("External OpenCode failed at " + method));
                    else resolve(reply.result);
                }
                replies.on("line", receive);
                external.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
            });
        }
        const externalInit = await externalRequest(1, "initialize", {
            protocolVersion: 1, clientCapabilities: {}, clientInfo: { name: "external-test", version: "1" },
        });
        assert.equal(externalInit.agentInfo.version, "1.19.6");
        const native = await externalRequest(2, "session/new", { cwd: workspace, mcpServers: [] });
        const importedArgs = { agent: "opencode", workspace, native_session_id: native.sessionId };
        assert.equal((await call("import_session", { ...importedArgs, native_session_id: "ses_missing_123456789" })).value.status,
            "native_session_not_found");
        assert.equal((await call("import_session", { ...importedArgs, workspace: otherWorkspace })).value.status,
            "native_session_not_found");
        assert.deepEqual((await call("list_sessions", {})).value, []);
        const imported = await call("import_session", importedArgs);
        assert(!imported.error, JSON.stringify(imported));
        assert.equal(imported.value.status, "pending_confirmation");
        assert.deepEqual((await call("import_session", importedArgs)).value, imported.value);
        assert.equal((await inspect(imported.value.session_id)).acp_session_id, native.sessionId);
        assert.equal(requests.length, 0, "Import sent a model prompt");

        const subscription = await rpc("events/subscribe", {
            name: "turn.completed", arguments: {},
            delivery: { mode: "webhook", url: callback, secret: "whsec_" + signingKey.toString("base64") },
        });
        assert(subscription.id);
        const streamed = await delegate("assignment-stream. Respond with owner-probe-pong.");
        assert(!streamed.error);
        const streamId = streamed.value.session_id;
        ownerPids.add((await deadline(async () => readOwner(streamId).catch(() => null),
            "Streaming owner did not start")).pid);
        await deadline(async () => {
            const record = await inspect(streamId);
            return record.result === "owner-probe-";
        }, "Scheduler did not receive the first streaming delta");
        const streamingSince = Date.now();
        while (Date.now() - streamingSince < 2800) {
            assert.equal((await inspect(streamId)).status, "running", scheduler.diagnostic);
            await pause(100);
        }
        assert.equal((await inspect(streamId)).status, "running");
        const observedFor = Date.now() - streamingSince;
        assert.equal(deliveries.length, 0);
        releaseStreaming();
        const streamComplete = await deadline(async () => {
            const record = await inspect(streamId);
            return record.status === "completed" && record;
        }, "Live streaming turn never completed");
        assert.equal(streamComplete.result, "owner-probe-pong");
        const streamDelivery = await deadline(() => deliveries[0], "Live turn emitted no signed completion");
        assert.equal(streamDelivery.name, "turn.completed");
        assert.equal(streamDelivery.data.session_id, streamId);
        assert.equal(streamDelivery.data.turn_id, streamed.value.turn_id);
        assert.equal(streamDelivery.data.reason, "end_turn");
        console.log("PASS live OpenCode stream stayed running for", observedFor,
            "ms and delivered one correlated completion without reconnect");

        const streamEndpoint = path.join(state, "owner-" + streamId + ".json");
        const streamOwner = await readOwner(streamId);
        await fs.writeFile(streamEndpoint, JSON.stringify({ ...streamOwner, port: 0 }));
        try {
            assert((await call("archive_session", { session_id: streamId })).error,
                "Archive reported success despite an unreachable owner");
            assert.equal((await inspect(streamId)).archived, false);
            assert(alive(streamOwner.pid));
        } finally {
            await fs.writeFile(streamEndpoint, JSON.stringify(streamOwner));
        }
        // Keep the isolated endpoint readable while temporarily denying deletion.
        const systemRoot = processEnv.SystemRoot ?? processEnv.SYSTEMROOT;
        const lock = start(path.join(systemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe"),
            ["-NoProfile", "-NonInteractive", "-Command",
                `$file = [IO.File]::Open($env:SS12_ENDPOINT, 'Open', 'Read', 'ReadWrite');
                [Console]::WriteLine('ready'); [void][Console]::ReadLine(); $file.Dispose()`],
            { SS12_ENDPOINT: streamEndpoint });
        const lockExit = once(lock, "exit");
        try {
            const [readyBytes] = await once(lock.stdout, "data", { signal: AbortSignal.timeout(8000) });
            assert.equal(readyBytes.toString().trim(), "ready");
            assert((await call("archive_session", { session_id: streamId })).error,
                "Archive acknowledged while owner endpoint cleanup was blocked");
            assert.equal((await inspect(streamId)).archived, false);
            assert(alive(streamOwner.pid));
        } finally {
            lock.stdin.end("\n");
            await lockExit;
        }
        await deadline(async () => !(await call("archive_session", { session_id: streamId })).error,
            "Archive did not succeed after endpoint cleanup was unblocked");
        assert(!(await fs.stat(streamEndpoint).then(() => true, () => false)),
            "Archive acknowledged before owner endpoint cleanup");
        await deadline(() => !alive(streamOwner.pid), "Streaming owner did not exit on successful retry");
        console.log("PASS unreachable owner and blocked endpoint cleanup left archive retryable, then confirmed clean shutdown");

        const first = await delegate("assignment-hold. Respond with owner-probe-pong.");
        assert(!first.error, JSON.stringify(first));
        const { session_id: id, turn_id: turn } = first.value;
        try {
            await deadline(() => requests.length > 1, "OpenCode did not reach the blocking model");
        } catch (error) {
            console.error("Isolated Scheduler", scheduler.diagnostic, "record", await inspect(id));
            throw error;
        }
        assert.equal(requests.length, 2);
        if (await readOwner(id).then(() => true, () => false)) ownerPids.add((await readOwner(id)).pid);
        const active = await inspect(id);
        assert.equal(active.status, "running");
        assert.match(active.acp_session_id, /^ses_/);
        assert((await call("archive_session", { session_id: id })).error, "Busy session was archived");
        const other = await delegate("assignment-counterpart. Respond with owner-probe-pong.");
        assert(!other.error, JSON.stringify(other));
        const otherId = other.value.session_id;
        await deadline(() => requests.some(text => text.includes("assignment-counterpart")),
            "Counterpart did not reach its blocking model");
        assert.equal(requests.length, 3);
        if (await readOwner(otherId).then(() => true, () => false)) ownerPids.add((await readOwner(otherId)).pid);
        const activeOther = await inspect(otherId);
        assert.equal(activeOther.status, "running");
        assert.match(activeOther.acp_session_id, /^ses_/);
        await stop(scheduler);
        scheduler = start(schedulerExe, [], environment);
        await ready();
        const restoredImport = await inspect(imported.value.session_id);
        assert.equal(restoredImport.acp_session_id, native.sessionId);
        assert.equal(restoredImport.status, "pending_confirmation");
        assert.deepEqual((await call("import_session", importedArgs)).value, imported.value);
        assert((await follow(imported.value.session_id, "blocked until verified")).error);
        console.log("PASS real external OpenCode import rejected missing and wrong-workspace IDs, preserved the verified ID across restart, and sent no prompt");
        const restored = await inspect(id);
        assert.equal(restored.acp_session_id, active.acp_session_id);
        assert.equal(restored.status, "pending_confirmation");
        assert((await follow(id, "forbidden-before-owner-evidence")).error);
        assert(await fs.stat(path.join(root, "scheduler-state", "owner-" + id + ".json")).then(() => true, () => false), "No durable OpenCode owner survived Scheduler restart");
        assert((await reconcile(id, turn)).error, "Busy session was reconciled");
        assert.equal(requests.length, 3);
        const restoredOther = await inspect(otherId);
        assert.equal(restoredOther.status, "pending_confirmation");
        assert.equal(restoredOther.acp_session_id, activeOther.acp_session_id);
        releaseBusy();
        const confirmed = await deadline(async () => {
            const response = await reconcile(id, turn);
            return !response.error && response.value;
        }, "Actual OpenCode owner never provided verifiable idle/turn evidence");
        assert.equal(confirmed.status, "completed");
        assert.equal(confirmed.reconciliation.source, "opencode_owner");
        assert.equal(requests.length, 3, "Reconciliation executed a turn");
        const busyOther = await reconcile(otherId, other.value.turn_id);
        assert(busyOther.error, "Reconciliation cleared a genuinely busy counterpart");
        assert((await follow(otherId, "forbidden-while-busy")).error);
        assert.equal(deliveries.length, 1, "Reconciliation emitted a completion");
        const next = await follow(id, "assignment-next. Respond with owner-probe-pong.");
        assert(!next.error, JSON.stringify(next));
        assert.notEqual(next.value.turn_id, turn);
        const completed = await deadline(async () => {
            const record = await inspect(id);
            return record.status === "completed" && record;
        }, "Follow-up did not complete");
        assert.equal(completed.acp_session_id, active.acp_session_id);
        assert.equal(completed.result, "owner-probe-pong");
        assert.equal(requests.length, 4);
        assert.match(requests[3], /assignment-hold.*assignment-next/, "Follow-up lacked native history");
        const delivery = await deadline(() => deliveries[1], "Ordinary follow-up did not emit its signed completion");
        assert.equal(delivery.name, "turn.completed");
        assert.equal(delivery.data.session_id, id);
        assert.equal(delivery.data.turn_id, next.value.turn_id);
        assert.equal(delivery.data.reason, "end_turn");

        releaseCounterpart();
        await deadline(async () => !(await reconcile(otherId, other.value.turn_id)).error,
            "Counterpart did not become idle after its model was released");
        const revoked = await call("revoke_workspace", { workspace });
        assert(!revoked.error);
        const revokedFollowUp = await follow(id, "forbidden-after-revocation");
        assert(revokedFollowUp.error);
        assert.equal(revokedFollowUp.value.status, "workspace_not_trusted");
        for (const sessionId of [id, otherId]) {
            const archived = await call("archive_session", { session_id: sessionId });
            assert(!archived.error, JSON.stringify(archived));
        }
        await deadline(() => [...ownerPids].every(pid => !alive(pid)), "Archived owners did not exit");
        await pause(1100);
        assert.equal(deliveries.length, 2, "An old turn or reconciliation emitted a duplicate completion");
        assert.equal(requests.length, 4);
        console.log("PASS same native IDs across restart; unverified and busy sends blocked; correlated idle; one new prompt and signed completion, no replay");
    } finally {
        releaseBusy();
        releaseCounterpart();
        releaseStreaming();
        for (const name of await fs.readdir(state).catch(() => [])) {
            if (!/^owner-[0-9a-f-]+\.json$/.test(name)) continue;
            const owner = await fs.readFile(path.join(state, name), "utf8").then(JSON.parse, () => null);
            if (owner) ownerPids.add(owner.pid);
        }
        await Promise.all(children.map(stop));
        for (const pid of ownerPids) {
            if (!alive(pid)) continue;
            await new Promise(resolve => execFile(path.join(processEnv.SystemRoot ?? processEnv.SYSTEMROOT,
                "System32", "taskkill.exe"), ["/PID", String(pid), "/T", "/F"], { windowsHide: true }, resolve));
        }
        for (const server of [model, events]) {
            server.closeAllConnections();
            await new Promise(resolve => server.close(resolve));
        }
        assert.equal(path.dirname(root), path.resolve(os.tmpdir()));
        await fs.rm(root, { recursive: true, force: true, maxRetries: 3 }).catch(error => {
            console.error("Isolated cleanup failed:", error.message);
        });
    }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
