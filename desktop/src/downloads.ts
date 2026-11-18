import type { Download, JobState, ManagedMedia, MediaItem } from './api'

export const MiB = 1_048_576
export const GiB = 1_073_741_824

export const waitingStates = new Set<JobState>(['queued', 'waiting_space', 'waiting_login', 'retry_wait'])
export const pauseStates = new Set<JobState>(['queued', 'downloading', 'waiting_space', 'retry_wait', 'waiting_login'])
export const resumeStates = new Set<JobState>(['paused', 'failed', 'retry_wait', 'waiting_login', 'cancelled', 'removed'])
export const reorderStates = new Set<JobState>([...waitingStates, 'paused', 'failed'])
export const jobStates: Record<JobState, string> = {
  queued: '排队中', downloading: '下载中', complete: '已缓存', paused: '已暂停', failed: '失败',
  waiting_space: '磁盘空间不足', waiting_login: '等待登录', retry_wait: '等待重试',
  deleting: '等待释放缓存', removed: '已清理', cancelled: '已取消',
}

export function size(bytes: number | null | undefined) {
  if (bytes == null) return '—'
  if (bytes < 1024) return `${bytes} B`
  if (bytes < MiB) return `${(bytes / 1024).toFixed(1)} KiB`
  if (bytes < GiB) return `${(bytes / MiB).toFixed(1)} MiB`
  return `${(bytes / GiB).toFixed(2)} GiB`
}

export function duration(seconds: number | null) {
  if (seconds == null) return ''
  if (seconds < 60) return `${seconds} 秒`
  if (seconds < 3600) return `约 ${Math.ceil(seconds / 60)} 分钟`
  return `约 ${(seconds / 3600).toFixed(1)} 小时`
}

export function downloadSpeed(jobs: Download[]) {
  return jobs.reduce((total, job) => total + (job.state === 'downloading' ? job.bytes_per_second : 0), 0)
}

export function mediaTitle(item: MediaItem) {
  const title = item.kind === 'Episode' ? item.tv_title || item.title : item.title
  return [title, episodeNumber(item)].filter(Boolean).join(' · ')
}

export function jobTitle(job: Download, managed: ManagedMedia[]) {
  return [...new Set(managed.map(({ item }) => mediaTitle(item) || item.guid))].join(' / ') || job.path
}

export function episodeNumber(item?: MediaItem) {
  if (item?.kind !== 'Episode' || !item.episode_number) return ''
  const episode = String(item.episode_number).padStart(2, '0')
  return item.season_number ? `S${String(item.season_number).padStart(2, '0')}E${episode}` : `第 ${item.episode_number} 集`
}
