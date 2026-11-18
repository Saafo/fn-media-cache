<script setup lang="ts">
import { computed, ref, shallowRef, watch } from 'vue'
import { desktopCall, errorText, get, jobAction, post } from './api'
import type { Observation, Routes, Snapshot } from './api'
import { size } from './downloads'

const props = defineProps<{ snapshot?: Snapshot; visible: boolean; canOpenLogs: boolean }>()
const emit = defineEmits<{ refresh: []; notice: [message: string] }>()
const routes = shallowRef<Routes>()
const observations = shallowRef<Observation[]>([])
const diagnostics = ref(false)
const readError = ref('')
const probing = ref(false)
const busy = ref('')
let reading = false
let generation = 0

const metrics = computed(() => {
  const stats = props.snapshot?.stats
  return [
    ['代理请求', stats?.requests ?? 0], ['本地供流', size(stats?.cache_bytes)], ['回源读取', size(stats?.origin_bytes)],
  ]
})
const playback = computed(() => props.snapshot?.stats.playback ?? [])
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
    const [nextRoutes, nextObservations] = await Promise.all([
      get('/routes'), diagnostics.value ? get('/observations') : Promise.resolve(undefined),
    ])
    if (current !== generation) return
    routes.value = nextRoutes
    if (nextObservations && diagnostics.value) observations.value = nextObservations
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
    observations.value = []
    readError.value = ''
  }
  void read()
})
watch(() => props.visible, read)
watch(diagnostics, read)

function playbackTitle(downloadId: string | null, path: string) {
  return props.snapshot?.automation.managed.find(entry => entry.download_id === downloadId)?.item.title || path
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

async function capture(item: Observation, sample: boolean) {
  if (busy.value) return
  busy.value = item.id
  emit('notice', '')
  try {
    const saved = await post('/downloads', { observation_id: item.id, ...(sample ? { paused: true } : {}) })
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
      <div class="overview-panels">
        <section class="panel">
          <div class="panel-head"><h2>播放读取</h2><small>本次运行</small></div>
          <p v-if="!playback.length" class="hint">暂无播放读取。</p>
          <table v-else aria-label="播放读取">
            <thead><tr><th scope="col">影片</th><th scope="col">状态</th><th scope="col">本地</th><th scope="col">回源</th></tr></thead>
            <tbody>
              <tr v-for="item in playback" :key="item.id || item.path">
                <td class="path">{{ playbackTitle(item.download_id, item.path) }}</td>
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
        <p v-if="!observations.length" class="hint">暂无代理请求。</p>
        <table v-else aria-label="代理请求">
          <thead><tr><th scope="col">路径</th><th scope="col">响应</th><th scope="col">大小</th><th scope="col">操作</th></tr></thead>
          <tbody id="observations">
            <tr v-for="item in observations" :key="item.id">
              <td class="path">{{ item.path }}</td><td>{{ item.status ?? '—' }}</td><td class="number">{{ size(item.total) }}</td>
              <td>
                <div v-if="item.cacheable" class="toolbar">
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
      </details>
    </template>
  </section>
</template>
