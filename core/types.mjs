/**
 * @typedef {object} Pool
 * @property {string} tag        runner tag and name suffix of the pool
 * @property {string} image      container image with the toolchain and the gitlab-runner binary
 * @property {string[]} stages   pipeline stages routed to this pool
 * @property {number} vcpu
 * @property {number} memoryMiB
 * @property {number} storageMiB
 * @property {number} timeoutS      hard limit of one run
 * @property {number} waitTimeoutS  idle seconds before a run exits
 * @property {number} maxRuns       live runs allowed at once
 */

/**
 * @typedef {object} Config
 * @property {string} name
 * @property {{ url: string, projectIds: number[] }} gitlab
 * @property {{ type: string } & Record<string, unknown>} host
 * @property {{ type: string } & Record<string, unknown>} provider
 * @property {string} defaultPool
 * @property {Pool[]} pools
 * @property {{ type: "none" | "s3", expireDays: number }} cache
 */

/**
 * @typedef {object} Secrets
 * @property {string} [hookToken]
 * @property {Record<string, string>} runnerTokens  pool tag to runner authentication token
 * @property {{ accessKey: string, secretKey: string }} [dispatch]
 * @property {{ accessKey: string, secretKey: string }} [cache]
 */

/**
 * @typedef {"create" | "update" | "delete" | "noop"} StepAction
 *
 * @typedef {object} Step
 * @property {string} id
 * @property {StepAction} action
 * @property {string} resource
 * @property {string} [detail]
 */

/**
 * @typedef {"queued" | "running" | "done" | "failed"} RunState
 *
 * @typedef {object} Run
 * @property {string} runId
 * @property {string} pool
 * @property {RunState} state
 * @property {string} createdAt
 * @property {Record<string, string>} labels
 */

/**
 * @typedef {object} Capabilities
 * @property {string} name
 * @property {boolean} dockerDaemon
 * @property {number} maxVcpu
 * @property {number} maxMemoryMiB
 * @property {number} maxStorageMiB
 * @property {number} maxDurationS
 * @property {string[]} archs
 * @property {"per-second" | "per-minute" | "per-hour"} billing
 */

/**
 * @typedef {"key" | "cache"} ProviderRotation
 *
 * @typedef {object} Provider
 * @property {() => Capabilities} capabilities
 * @property {(options?: { rotate?: ProviderRotation[] }) => Promise<Step[]>} plan
 * @property {(steps: Step[]) => Promise<Record<string, string>>} apply   outputs; secret values appear only when created or rotated
 * @property {() => Promise<void>} destroy
 * @property {(req: { pool: Pool, env: Record<string, string>, labels: Record<string, string> }) => Promise<{ runId: string }>} start
 * @property {(filter?: { pool?: string }) => Promise<Run[]>} list
 * @property {(runId: string) => Promise<void>} stop
 */

/**
 * @typedef {object} Host
 * @property {(input: { secrets: Secrets }) => Promise<Step[]>} plan
 * @property {(steps: Step[], input: { secrets: Secrets }) => Promise<Record<string, string>>} apply
 * @property {() => Promise<void>} destroy
 * @property {() => Promise<string | undefined>} url
 */

/**
 * @typedef {object} DispatchRequest
 * @property {Record<string, string>} headers
 * @property {string | object} body
 *
 * @typedef {object} DispatchResponse
 * @property {number} status
 * @property {Record<string, unknown>} body
 */

export {};
