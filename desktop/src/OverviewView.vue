<script setup lang="ts">
import { computed, ref, shallowRef, watch } from 'vue'
import { desktopCall, errorText, get, jobAction, post } from './api'
import type { JobState, MediaItem, ProxyRequest, Routes, Snapshot, Stats } from './api'
import { downloadSpeed, mediaTitle, size, waitingStates } from './downloads'

const props = defineProps<{ snapshot?: Snapshot; visible: boolean; canOpenLogs: boolean }>()
const emit = defineEmits<{ refresh: []; notice: [message: string]; showTasks: [] }>()
const routes = shallowRef<Routes>()
const requests = shallowRef<ProxyRequest[]>([])
const diagnostics = ref(false)
const readError = ref('')
const probing = ref(false)
const busy = ref('')
const requestTime = new Intl.DateTimeFormat(undefined, {
  hour: '2-digit', minute: '2-digit', second: '2-digit', fractionalSecondDigits: 3, hourCycle: 'h23',
})
let reading = false
let generation = 0

const metrics = computed(() => {
  const stats = props.snapshot?.stats
  return [
    ['代理请求', stats?.requests ?? 0], ['本地供流', size(stats?.cache_bytes)], ['回源读取', size(stats?.origin_bytes)],
  ]
})
const playback = computed(() => props.snapshot?.stats.playback ?? [])
const media = computed(() => props.snapshot?.automation.managed ?? [])
const rangePrefix = '/v/api/v1/media/range/'
const readTotals = computed(() => playback.value.reduce((totals, reading) => {
  totals.local += reading.local_bytes
  totals.origin += reading.upstream_bytes
  return totals
}, { local: 0, origin: 0 }))
const bytesRead = computed(() => readTotals.value.local + readTotals.value.origin)
const localShare = computed(() => bytesRead.value ? 100 * readTotals.value.local / bytesRead.value : 0)
const downloadRate = computed(() => downloadSpeed(props.snapshot?.downloads ?? []))
const downloadSummary = computed(() => {
  const counts = new Map<JobState, number>()
  for (const { state } of props.snapshot?.downloads ?? []) counts.set(state, (counts.get(state) ?? 0) + 1)
  return [
    { label: '下载中', color: 'active', count: counts.get('downloading') ?? 0 },
    { label: '等待中', color: 'waiting', count: [...waitingStates].reduce((sum, state) => sum + (counts.get(state) ?? 0), 0) },
    { label: '已暂停', color: 'paused', count: counts.get('paused') ?? 0 },
    { label: '已缓存', color: 'cached', count: counts.get('complete') ?? 0 },
    { label: '失败', color: 'failed', count: counts.get('failed') ?? 0 },
  ]
})
const routeNames = { unmeasured: '未测速', ready: '可用', failed: '失败', mismatch: '内容待验证' }
const routeSummary = computed(() => {
  if (probing.value || routes.value?.measuring) return '测速中…'
  if (!routes.value?.measured_at_ms) return '等待视频请求后测速。'
  return `上次测速 ${new Date(routes.value.measured_at_ms).toLocaleTimeString()}`
})

async function read() {
  if (reading || !props.visible || !props.snapshot) return
  reading = true
  const current = generation
  try {
    const [nextRoutes, nextRequests] = await Promise.all([
      get('/routes'), diagnostics.value ? get('/proxy-requests') : Promise.resolve(undefined),
    ])
    if (current !== generation) return
    routes.value = nextRoutes
    if (nextRequests && diagnostics.value) requests.value = nextRequests
    readError.value = ''
  } catch (error) {
    if (current === generation && props.visible) readError.value = errorText(error)
  } finally {
    reading = false
  }
}

watch(() => props.snapshot, value => {
  if (!value) {
    generation++
    routes.value = undefined
    requests.value = []
    readError.value = ''
  }
  void read()
})
watch(() => props.visible, read)
watch(diagnostics, read)

function unambiguousTitle(items: MediaItem[]) {
  // A shared file can be linked to multiple catalog entries.
  if (items.length && items.every(item => item.guid === items[0].guid)) {
    return mediaTitle(items.find(item => item.tv_title && item.episode_number) ?? items.find(item => item.episode_number) ?? items[0])
  }
  const names = [...new Set(items.map(mediaTitle).filter(Boolean))]
  return names.length === 1 ? names[0] : ''
}

function playbackTitle(reading: Stats['playback'][number]) {
  const { path, download_id, item } = reading
  if (item && (item.kind !== 'Episode' || item.episode_number)) {
    const name = mediaTitle(item)
    if (name) return name
  }
  const attached = download_id ? media.value.filter(entry => entry.download_id === download_id) : []
  const direct = unambiguousTitle(attached.map(entry => entry.item))
  if (direct) return direct
  if (path.startsWith(rangePrefix)) {
    const guid = path.slice(rangePrefix.length)
    const matching = media.value.filter(entry => entry.media_guid === guid).map(entry => entry.item)
    return unambiguousTitle(matching) || (item ? mediaTitle(item) : '') || '未识别影片'
  }
  if (item && mediaTitle(item)) return mediaTitle(item)
  const basename = path.split('/').pop() || path
  let name = basename
  try { name = decodeURIComponent(basename) } catch { /* Keep the raw filename if it is malformed. */ }
  const episode = /^(.+?)[ ._-]+s(\d{1,3})[ ._-]*e(\d{1,4})(?=[ ._-]|$)/i.exec(name)
  return episode ? `${episode[1].replace(/[._]+/g, ' ')} · S${episode[2].padStart(2, '0')}E${episode[3].padStart(2, '0')}` : name
}

async function probe() {
  if (probing.value) return
  probing.value = true
  emit('notice', '')
  try {
    await post('/routes/probe', {})
    emit('notice', '测速完成。')
  } catch (error) {
    emit('notice', errorText(error))
  } finally {
    probing.value = false
    void read()
  }
}

async function openLogs() {
  try {
    await desktopCall('open_logs', undefined)
  } catch (error) {
    emit('notice', errorText(error))
  }
}

async function capture(item: ProxyRequest, sample: boolean) {
  if (busy.value || !item.observation_id) return
  busy.value = item.observation_id
  emit('notice', '')
  try {
    const saved = await post('/downloads', { observation_id: item.observation_id, ...(sample ? { paused: true } : {}) })
    const job = (await get('/downloads')).find(value => value.id === saved.id)
    if (sample) {
      if (job && ['queued', 'downloading'].includes(job.state)) {
        emit('notice', '已有完整下载任务。')
        return
      }
      const result = await post(`/downloads/${saved.id}/cache-samples`, {})
      emit('notice', `已缓存 ${size(result.downloaded_bytes)} 样本。`)
    } else {
      if (job && ['paused', 'failed', 'cancelled', 'removed'].includes(job.state)) await jobAction(job, 'resume')
      emit('notice', job?.state === 'complete' ? '已完整缓存。' : '已加入队列。')
    }
    emit('refresh')
  } catch (error) {
    emit('notice', errorText(error))
  } finally {
    busy.value = ''
  }
}
</script>

<template>
  <section data-view="overview">
    <header class="page-head">
      <h1 tabindex="-1">运行总览</h1>
      <button v-if="canOpenLogs" id="service-logs" type="button" @click="openLogs">打开日志目录</button>
    </header>
    <template v-if="snapshot">
      <div class="panel stats" aria-label="本次运行统计">
        <div v-for="[label, value] in metrics" :key="label"><span>{{ label }}</span><strong>{{ value }}</strong></div>
      </div>
      <section class="panel download-overview" aria-labelledby="download-overview-title">
        <div class="panel-head">
          <h2 id="download-overview-title">下载状态</h2>
          <div class="download-overview-meta">
            <p class="download-rate"><span class="hint">下载速度</span><strong class="number">{{ size(downloadRate) }}/s</strong></p>
            <button type="button" @click="emit('showTasks')">查看任务</button>
          </div>
        </div>
        <div class="data-strip" aria-hidden="true">
          <span v-for="status in downloadSummary" :key="status.color"
            :class="`chart-${status.color}`" :style="{ flexGrow: status.count }"></span>
        </div>
        <dl class="download-statuses">
          <div v-for="status in downloadSummary" :key="status.color">
            <dt><span class="chart-key" :class="`chart-${status.color}`"></span>{{ status.label }}</dt>
            <dd class="number">{{ status.count }}</dd>
          </div>
        </dl>
      </section>
      <div class="overview-panels">
        <section class="panel">
          <div class="panel-head"><h2>播放读取</h2><small>本次运行</small></div>
          <p v-if="!playback.length" class="hint">暂无播放读取。</p>
          <div v-if="bytesRead" class="playback-mix">
            <div class="read-legend hint">
              <span><span class="chart-key chart-cached"></span>本地 {{ size(readTotals.local) }}</span>
              <strong class="number">{{ Math.round(localShare) }}% 本地</strong>
              <span><span class="chart-key chart-active"></span>回源 {{ size(readTotals.origin) }}</span>
            </div>
            <div class="data-strip" aria-hidden="true">
              <span class="chart-cached" :style="{ flexGrow: localShare }"></span>
              <span class="chart-active" :style="{ flexGrow: 100 - localShare }"></span>
            </div>
          </div>
          <table v-if="playback.length" aria-label="播放读取">
            <thead><tr><th scope="col">影片</th><th scope="col">状态</th><th scope="col">本地</th><th scope="col">回源</th></tr></thead>
            <tbody>
              <tr v-for="item in playback" :key="item.id || item.path">
                <td class="path" :title="item.path">{{ playbackTitle(item) }}</td>
                <td>{{ item.active_requests ? '读取中' : '最近读过' }}</td>
                <td class="number">{{ size(item.local_bytes) }}</td>
                <td class="number">{{ size(item.upstream_bytes) }}</td>
              </tr>
            </tbody>
          </table>
        </section>
        <section class="panel">
          <div class="panel-head">
            <h2>下载线路</h2>
            <button id="probe-routes" type="button" :disabled="probing || routes?.measuring" @click="probe">重新测速</button>
          </div>
          <p class="hint">{{ routeSummary }}</p>
          <table aria-label="下载线路">
            <thead><tr><th scope="col">地址</th><th scope="col">状态</th><th scope="col">样本速度</th><th scope="col">耗时</th></tr></thead>
            <tbody id="routes">
              <tr v-for="route in routes?.results" :key="route.origin" :title="route.error || undefined">
                <td class="path">{{ route.origin }}</td>
                <td>{{ route.origin === routes?.selected ? '首选' : routeNames[route.status] }}</td>
                <td class="number">{{ route.mib_per_second == null ? '—' : route.mib_per_second.toFixed(2) + ' MiB/s' }}</td>
                <td class="number">{{ route.elapsed_ms == null ? '—' : route.elapsed_ms + ' ms' }}</td>
              </tr>
            </tbody>
          </table>
        </section>
      </div>
      <p v-if="readError" id="overview-error" class="hint notice" role="status">{{ readError }}</p>
      <details id="diagnostics" class="panel diagnostics" @toggle="diagnostics = ($event.currentTarget as HTMLDetailsElement).open">
        <summary>诊断</summary>
        <p class="hint">缓存命中 {{ snapshot.stats.cache_hits }} 次 · 跨域跳转 {{ snapshot.stats.external_redirects }} 次</p>
        <h2>最近代理请求</h2>
        <p v-if="!requests.length" class="hint">暂无代理请求。</p>
        <div v-else class="request-scroll">
          <table aria-label="代理请求">
            <thead><tr><th scope="col">时间</th><th scope="col">设备 IP</th><th scope="col">Method</th><th scope="col">路径</th><th scope="col">响应</th><th scope="col">大小</th><th scope="col">操作</th></tr></thead>
            <tbody id="proxy-requests">
              <tr v-for="item in requests" :key="item.id">
                <td class="number"><time :datetime="new Date(item.timestamp_ms).toISOString()" :title="new Date(item.timestamp_ms).toLocaleString()">{{ requestTime.format(item.timestamp_ms) }}</time></td>
                <td class="number">{{ item.client_ip ?? '—' }}</td>
                <td>{{ item.method }}</td>
                <td class="path">{{ item.path }}</td>
                <td>{{ item.status ?? (item.pending ? '等待响应' : '连接已关闭') }}</td>
                <td class="number">{{ size(item.total) }}</td>
                <td>
                  <div v-if="item.observation_id" class="toolbar">
                    <button
                      v-if="item.path.startsWith('/v/api/v1/media/range/')"
                      type="button" title="最多读取 3 MiB" :disabled="Boolean(busy)" @click="capture(item, true)"
                    >缓存样本</button>
                    <button type="button" :disabled="Boolean(busy)" @click="capture(item, false)">完整下载</button>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </details>
    </template>
  </section>
</template>
