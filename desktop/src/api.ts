export type JobState =
  | 'queued' | 'downloading' | 'complete' | 'paused' | 'failed'
  | 'waiting_space' | 'waiting_login' | 'retry_wait' | 'deleting' | 'removed' | 'cancelled'

export interface Download {
  id: string
  path: string
  state: JobState
  downloaded: number
  total: number | null
  bytes_per_second: number
  eta_seconds: number | null
  retry_at: number | null
  waiting_reason: 'playback' | null
  error: string | null
}

export interface MediaItem {
  guid: string
  title: string
  kind: string
  parent_guid?: string
  tv_title?: string
  season_number: number
  episode_number: number
  watched?: boolean | null
}

export interface ManagedMedia {
  download_id: string
  session_id: string
  item: MediaItem
  media_guid?: string
  keep: boolean
  completed_at: number | null
}

export interface Policy {
  auto_prefetch: boolean
  incognito: boolean
  local_history: boolean
  prefetch_count: number
  prefetch_after_seconds?: number
  bytes_per_second: number
  pause_during_playback: boolean
  prevent_idle_sleep: boolean
  cleanup_enabled: boolean
  cleanup_previous_episodes?: boolean
  retention_seconds: number | null
  playback_cooldown_seconds: number
  reserved_disk_bytes: number
  cache_limit_bytes: number | null
  scheduler_interval_seconds: number
  route_result_ttl_seconds: number
  route_timeout_seconds: number
  route_sample_bytes: number
  retry_max_attempts: number
  retry_base_seconds: number
  retry_max_seconds: number
  session_check_seconds: number
  resume_detection_seconds: number
}

export interface Automation {
  policy: Policy
  managed: ManagedMedia[]
  playing: boolean
  downloads_yielding?: boolean
  prevent_idle_sleep_supported?: boolean
  cross_season_cleanup_supported?: boolean
  optional_retention_supported?: boolean
  cache_limit_bytes: number
}

export interface Stats {
  requests: number
  cache_hits: number
  cache_bytes: number
  origin_bytes: number
  external_redirects: number
  playback_totals?: { local_bytes: number; upstream_bytes: number }
  playback: {
    id: string
    download_id: string | null
    path: string
    item?: MediaItem
    active_requests: number
    local_bytes: number
    upstream_bytes: number
    read_coverage?: string
  }[]
}

export interface Snapshot {
  stats: Stats
  downloads: Download[]
  automation: Automation
}

export interface Routes {
  measuring: boolean
  selected: string | null
  measured_at_ms: number | null
  results: {
    origin: string
    status: 'unmeasured' | 'ready' | 'failed' | 'mismatch'
    mib_per_second: number | null
    elapsed_ms: number | null
    error: string | null
  }[]
}

export interface ProxyRequest {
  id: number
  timestamp_ms: number
  method: string
  client_ip?: string
  path: string
  status: number | null
  total: number | null
  observation_id: string | null
  pending: boolean
}

export interface Session {
  id: string
  origin: string
  status: string
}

export interface CatalogPage {
  items: MediaItem[]
  has_more: boolean
}

export interface ServerSettings {
  upstream: string
  upstream_alternates: string[]
  proxy_port: number
  lan_access: boolean
  log_max_megabytes: number
  log_keep_files: number
}

export interface DesktopStatus {
  configured: boolean
  running: boolean
  settings: ServerSettings
  proxy_address: string
  admin_address: string
  last_error: string | null
}

export type DownloadCommand = 'pause' | 'resume' | 'cancel' | 'delete' | 'up' | 'down' | 'first'

interface Reads {
  '/stats': Stats
  '/downloads': Download[]
  '/automation': Automation
  '/routes': Routes
  '/proxy-requests': ProxyRequest[]
  '/sessions': Session[]
}

interface Writes {
  '/downloads': [{ observation_id: string; paused?: boolean }, { id: string }]
  '/automation/policy': [Omit<Policy, 'prevent_idle_sleep'> & { prevent_idle_sleep?: boolean }, { ok: boolean }]
  '/library/search': [{ session_id: string; page: number; query: string }, CatalogPage]
  '/library/browse': [{ session_id: string; page: number; item_guid: string }, CatalogPage]
  '/library/series': [{ session_id: string; item_guid: string }, MediaItem]
  '/library/download': [
    { session_id: string; item_guids: string[] },
    { results: { item_guid: string; ok: boolean; id?: string }[] },
  ]
  '/managed/retention': [{ session_id: string; item_guid: string; keep?: boolean; completed?: boolean }, { ok: boolean }]
  '/routes/probe': [Record<string, never>, unknown]
  [path: `/downloads/${string}/cache-samples`]: [Record<string, never>, { downloaded_bytes: number }]
  [path: `/downloads/${string}/${DownloadCommand}`]: [Record<string, never>, { ok: boolean }]
}

interface DesktopCalls {
  service_status: [undefined, DesktopStatus]
  save_server: [{ settings: ServerSettings }, DesktopStatus]
  open_logs: [undefined, null]
  admin_request: [{ path: string; body: unknown }, { status: number; body: unknown }]
}

declare global {
  interface Window {
    __TAURI__?: { core: { invoke: <T>(command: string, args?: Record<string, unknown>) => Promise<T> } }
    __FN_MEDIA_CACHE_OVERLAY__?: boolean
  }
}

const bridge = window.__TAURI__?.core
let token = ''

export const isDesktop = Boolean(bridge)
export const hasOverlayTitlebar = isDesktop && window.__FN_MEDIA_CACHE_OVERLAY__ === true

export function storeToken(value: string) {
  token = value
}

export function desktopCall<K extends keyof DesktopCalls>(
  command: K,
  args: DesktopCalls[K][0],
): Promise<DesktopCalls[K][1]> {
  if (!bridge) throw new Error('桌面服务不可用。')
  return bridge.invoke(command, args)
}

export class ApiError extends Error {
  constructor(public readonly status: number, body: unknown) {
    const detail = typeof body === 'string' && /^[\u3400-\u9fff]/.test(body) ? body : ''
    const message = status === 401 ? '管理令牌无效。' : `请求失败（${status}）`
    super(detail || message)
  }
}

export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

async function request<T>(path: string, body?: unknown): Promise<T> {
  if (bridge) {
    const result = await desktopCall('admin_request', { path, body: body ?? null })
    if (result.status < 200 || result.status >= 300) throw new ApiError(result.status, result.body)
    return result.body as T
  }
  const response = await fetch(`/api${path}`, {
    method: body === undefined ? 'GET' : 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  }).catch(() => { throw new Error('连不上后台。') })
  if (!response.ok) throw new ApiError(response.status, await response.text())
  return response.json() as Promise<T>
}

export function get<K extends keyof Reads>(path: K) {
  return request<Reads[K]>(path)
}

export function post<K extends keyof Writes>(path: K, body: Writes[K][0]) {
  return request<Writes[K][1]>(path, body)
}

export function jobAction(job: Download, command: DownloadCommand) {
  return post(`/downloads/${job.id}/${command}`, {})
}
