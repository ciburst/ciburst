export const FAKE_TOKEN = "glpat-test";

/**
 * Fetch-level fake of the GitLab endpoints ciburst uses.
 * @param {{ jobs?: Record<number, object[]>, lintErrors?: Record<number, string[]> }} [options]
 */
export function createFakeGitlab({ jobs = {}, lintErrors = {} } = {}) {
  const state = { runners: [], hooks: [], nextId: 100, calls: [] };

  const json = (status, data) =>
    new Response(JSON.stringify(data), { status, headers: { "content-type": "application/json" } });
  const empty = () => new Response(null, { status: 204 });

  async function fetch(input, init = {}) {
    const url = new URL(String(input));
    const method = init.method ?? "GET";
    const path = url.pathname.replace(/^\/api\/v4/, "");
    const body = init.body ? JSON.parse(init.body) : undefined;
    state.calls.push({ method, path, body });
    if (init.headers?.["PRIVATE-TOKEN"] !== FAKE_TOKEN)
      return json(401, { message: "401 Unauthorized" });

    let match;
    if ((match = path.match(/^\/projects\/(\d+)\/runners$/))) {
      const projectId = Number(match[1]);
      if (method === "GET") {
        return json(
          200,
          state.runners.filter((r) => r.projects.includes(projectId)).map(publicRunner),
        );
      }
      if (method === "POST") {
        const runner = state.runners.find((r) => r.id === body.runner_id);
        if (!runner) return json(404, { message: "404 Runner Not Found" });
        if (!runner.projects.includes(projectId)) runner.projects.push(projectId);
        return json(201, publicRunner(runner));
      }
    }
    if (path === "/user/runners" && method === "POST") {
      const runner = {
        id: state.nextId++,
        description: body.description,
        tag_list: body.tag_list,
        run_untagged: body.run_untagged,
        runner_type: body.runner_type,
        projects: [body.project_id],
      };
      state.runners.push(runner);
      return json(201, { id: runner.id, token: `glrt-fake-${runner.id}`, token_expires_at: null });
    }
    if ((match = path.match(/^\/runners\/(\d+)$/)) && method === "DELETE") {
      const before = state.runners.length;
      state.runners = state.runners.filter((r) => r.id !== Number(match[1]));
      return before === state.runners.length ? json(404, { message: "404 Not found" }) : empty();
    }
    if ((match = path.match(/^\/projects\/(\d+)\/hooks$/))) {
      const projectId = Number(match[1]);
      if (method === "GET")
        return json(200, state.hooks.filter((h) => h.project_id === projectId).map(publicHook));
      if (method === "POST") {
        const hook = { id: state.nextId++, project_id: projectId, ...hookFields(body) };
        state.hooks.push(hook);
        return json(201, publicHook(hook));
      }
    }
    if ((match = path.match(/^\/projects\/(\d+)\/hooks\/(\d+)$/))) {
      const hook = state.hooks.find(
        (h) => h.project_id === Number(match[1]) && h.id === Number(match[2]),
      );
      if (!hook) return json(404, { message: "404 Not found" });
      if (method === "PUT") {
        Object.assign(hook, hookFields(body));
        return json(200, publicHook(hook));
      }
      if (method === "DELETE") {
        state.hooks = state.hooks.filter((h) => h !== hook);
        return empty();
      }
    }
    if ((match = path.match(/^\/projects\/(\d+)\/ci\/lint$/)) && method === "GET") {
      const projectId = Number(match[1]);
      const errors = lintErrors[projectId] ?? [];
      const valid = errors.length === 0;
      return json(200, { valid, errors, warnings: [], jobs: valid ? (jobs[projectId] ?? []) : [] });
    }
    return json(404, { message: `404 no route for ${method} ${path}` });
  }

  return { fetch, state };
}

function hookFields(body) {
  return {
    url: body.url,
    token: body.token,
    job_events: body.job_events,
    push_events: body.push_events,
    enable_ssl_verification: body.enable_ssl_verification,
  };
}

function publicRunner(runner) {
  const { projects, ...rest } = runner;
  return rest;
}

function publicHook(hook) {
  const { token, ...rest } = hook;
  return rest;
}
