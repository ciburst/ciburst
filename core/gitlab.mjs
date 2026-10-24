export class GitlabError extends Error {
  constructor(status, method, path, body) {
    const message = typeof body === "string" ? body : (body?.message ?? body?.error ?? "");
    super(`gitlab ${method} ${path}: ${status} ${JSON.stringify(message)}`);
    this.name = "GitlabError";
    this.status = status;
  }
}

/**
 * Minimal GitLab REST client for the resources ciburst manages.
 * @param {{ url: string, token: string, fetch?: typeof fetch }} options
 */
export function createGitlab({ url, token, fetch = globalThis.fetch }) {
  const base = `${url.replace(/\/+$/, "")}/api/v4`;

  async function api(method, path, { query, body } = {}) {
    const target = new URL(base + path);
    for (const [key, value] of Object.entries(query ?? {}))
      target.searchParams.set(key, String(value));
    const response = await fetch(target, {
      method,
      headers: {
        "PRIVATE-TOKEN": token,
        accept: "application/json",
        "content-type": "application/json",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    const data = text ? parseJson(text) : null;
    if (!response.ok) throw new GitlabError(response.status, method, path, data ?? text);
    return data;
  }

  const hookBody = (hookUrl, hookToken) => ({
    url: hookUrl,
    token: hookToken,
    job_events: true,
    push_events: false,
    enable_ssl_verification: true,
  });

  return {
    listRunners: (projectId) =>
      api("GET", `/projects/${projectId}/runners`, {
        query: { type: "project_type", per_page: 100 },
      }),
    createRunner: ({ projectId, tag, description }) =>
      api("POST", "/user/runners", {
        body: {
          runner_type: "project_type",
          project_id: projectId,
          tag_list: [tag],
          run_untagged: false,
          description,
        },
      }),
    enableRunner: (projectId, runnerId) =>
      api("POST", `/projects/${projectId}/runners`, { body: { runner_id: runnerId } }),
    deleteRunner: (runnerId) => api("DELETE", `/runners/${runnerId}`),
    listHooks: (projectId) =>
      api("GET", `/projects/${projectId}/hooks`, { query: { per_page: 100 } }),
    createHook: ({ projectId, url: hookUrl, token: hookToken }) =>
      api("POST", `/projects/${projectId}/hooks`, { body: hookBody(hookUrl, hookToken) }),
    updateHook: ({ projectId, hookId, url: hookUrl, token: hookToken }) =>
      api("PUT", `/projects/${projectId}/hooks/${hookId}`, { body: hookBody(hookUrl, hookToken) }),
    deleteHook: (projectId, hookId) => api("DELETE", `/projects/${projectId}/hooks/${hookId}`),
    lintJobs: (projectId) =>
      api("GET", `/projects/${projectId}/ci/lint`, { query: { include_jobs: true } }),
  };
}

function parseJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}
