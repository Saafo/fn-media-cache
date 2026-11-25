<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'
import { desktopCall, errorText, isDesktop, post } from './api'
import type { DesktopStatus, Policy, ServerSettings, Snapshot } from './api'
import { GiB, MiB } from './downloads'

const props = defineProps<{ desktop?: DesktopStatus; snapshot?: Snapshot }>()
const emit = defineEmits<{ refresh: []; serverSaved: [status: DesktopStatus] }>()
const mode = ref<'sync' | 'local' | 'incognito'>('sync')
const server = ref<ServerSettings>()
const alternates = ref('')
const policy = ref<Policy>()
const retentionHours = ref<number | ''>('')
const policyMessage = ref('')
const serverMessage = ref('')
const savingPolicy = ref(false)
const savingServer = ref(false)
const savedPolicy = ref<Policy>()
const savedServer = ref('')

type NumericKey = Exclude<keyof Policy, 'incognito' | 'local_history' | 'auto_prefetch' | 'pause_during_playback' | 'prevent_idle_sleep' | 'cleanup_enabled' | 'cleanup_previous_episodes' | 'prefetch_after_seconds' | 'retention_seconds'>
type NumberField = [key: NumericKey, label: string, min: number, max: number | undefined, scale?: number]
const downloadFields: NumberField[] = [
  ['bytes_per_second', '限速（MiB/s，0 为不限速）', 0, 1024, MiB],
  ['playback_cooldown_seconds', '回源结束后等待（秒）', 0, 3600],
  ['prefetch_count', '预取集数', 1, 20],
]
const cacheFields: NumberField[] = [
  ['cache_limit_bytes', '上限（GiB）', 0.01, undefined, GiB],
  ['reserved_disk_bytes', '磁盘预留（MiB）', 0, undefined, MiB],
]
const advancedFields: NumberField[] = [
  ['scheduler_interval_seconds', '调度间隔（秒）', 1, 3600],
  ['route_result_ttl_seconds', '测速结果有效期（分钟）', 0.5, 1440, 60],
  ['route_timeout_seconds', '线路测速超时（秒）', 1, 120],
  ['route_sample_bytes', '线路测速样本（KiB）', 1, 1024, 1024],
  ['retry_max_attempts', '自动重试次数', 0, 100],
  ['retry_base_seconds', '首次重试间隔（秒）', 1, 86400],
  ['retry_max_seconds', '最长重试间隔（秒）', 1, 86400],
  ['session_check_seconds', '会话检查间隔（秒）', 1, 3600],
  ['resume_detection_seconds', '恢复检测阈值（秒）', 1, 3600],
]
const fields = [...downloadFields, ...cacheFields, ...advancedFields]
const numbers = reactive<Partial<Record<NumericKey, string | number>>>({})
const optionalRetention = computed(() => props.snapshot?.automation.optional_retention_supported === true)
const policyPayload = computed(() => {
  if (!policy.value) return
  const payload = {
    ...policy.value,
    incognito: mode.value === 'incognito',
    local_history: mode.value === 'local',
    cleanup_enabled: optionalRetention.value || policy.value.cleanup_enabled,
    retention_seconds: retentionHours.value === '' ? null : Math.round(retentionHours.value * 3600),
  }
  for (const [key, , , , scale = 1] of fields) payload[key] = Math.round(Number(numbers[key]) * scale)
  return payload
})
const serverPayload = computed(() => server.value && normalizeServer(server.value, alternates.value.split('\n')))
const policyChanged = computed(() => Boolean(policyPayload.value) && JSON.stringify(policyPayload.value) !== JSON.stringify(savedPolicy.value))
const serverChanged = computed(() => Boolean(serverPayload.value) && JSON.stringify(serverPayload.value) !== savedServer.value)
const historyNote = computed(() => ({
  sync: '',
  local: '最近播放和续播位置只保存在本机。',
  incognito: '停止上报播放记录。关闭前请先退出播放页，避免补报。',
})[mode.value])

function normalizeServer(settings: ServerSettings, addresses: string[]): ServerSettings {
  return {
    ...settings,
    upstream: settings.upstream.trim().replace(/\/+$/, ''),
    upstream_alternates: addresses.map(value => value.trim().replace(/\/+$/, '')).filter(Boolean),
  }
}

function assignServer(settings: ServerSettings) {
  server.value = { ...settings }
  alternates.value = settings.upstream_alternates.join('\n')
  savedServer.value = JSON.stringify(serverPayload.value)
}

watch(() => props.desktop?.settings, settings => {
  if (settings && !server.value) assignServer(settings)
}, { immediate: true })

watch(() => props.snapshot, snapshot => {
  if (!snapshot) {
    policy.value = undefined
    return
  }
  const { automation } = snapshot
  if (policy.value && savedPolicy.value) {
    if (!savingPolicy.value) {
      const enabled = automation.policy.prevent_idle_sleep
      if (policy.value.prevent_idle_sleep === savedPolicy.value.prevent_idle_sleep) {
        policy.value.prevent_idle_sleep = enabled
      }
      savedPolicy.value.prevent_idle_sleep = enabled
    }
    return
  }
  const values = { ...automation.policy, cache_limit_bytes: automation.cache_limit_bytes }
  if (optionalRetention.value && !values.cleanup_enabled) {
    values.cleanup_previous_episodes = false
    values.retention_seconds = null
  }
  policy.value = values
  retentionHours.value = values.retention_seconds === null ? '' : values.retention_seconds / 3600
  mode.value = values.incognito ? 'incognito' : values.local_history ? 'local' : 'sync'
  for (const [key, , , , scale = 1] of fields) numbers[key] = values[key] / scale
  savedPolicy.value = policyPayload.value
}, { immediate: true })

watch(policyPayload, () => {
  if (policyChanged.value && !savingPolicy.value) policyMessage.value = ''
})
watch(serverPayload, () => {
  if (serverChanged.value && !savingServer.value) serverMessage.value = ''
})

async function savePolicy() {
  const payload = policyPayload.value
  if (savingPolicy.value || !payload || !policyChanged.value) return
  savingPolicy.value = true
  policyMessage.value = ''
  try {
    const { prevent_idle_sleep, ...otherPreferences } = payload
    await post('/automation/policy', prevent_idle_sleep === savedPolicy.value?.prevent_idle_sleep ? otherPreferences : payload)
    savedPolicy.value = payload
    policyMessage.value = policyChanged.value ? '' : '已保存。'
    emit('refresh')
  } catch (error) {
    policyMessage.value = errorText(error)
  } finally {
    savingPolicy.value = false
  }
}

async function saveServer() {
  const settings = serverPayload.value
  if (savingServer.value || !settings || !serverChanged.value) return
  savingServer.value = true
  serverMessage.value = ''
  try {
    const status = await desktopCall('save_server', { settings })
    savedServer.value = JSON.stringify(normalizeServer(status.settings, status.settings.upstream_alternates))
    if (JSON.stringify(serverPayload.value) === JSON.stringify(settings)) {
      assignServer(status.settings)
      serverMessage.value = '已保存。'
    }
    emit('serverSaved', status)
  } catch (error) {
    serverMessage.value = errorText(error)
  } finally {
    savingServer.value = false
  }
}
</script>

<template>
  <section data-view="settings">
    <header class="page-head"><h1 tabindex="-1">设置</h1></header>
    <form v-if="isDesktop && server" id="desktop-panel" class="panel" @submit.prevent="saveServer">
      <h2>服务器</h2>
      <div class="settings">
        <label class="wide">主地址<input v-model="server.upstream" type="url" placeholder="http://服务器:端口" required></label>
        <label class="wide">备用地址（每行一个）<textarea v-model="alternates" rows="2"></textarea></label>
        <label>代理端口<input v-model.number="server.proxy_port" type="number" min="1" max="65535" required></label>
        <label>单份日志上限（MiB）<input v-model.number="server.log_max_megabytes" type="number" min="1" max="1024" required></label>
        <label>保留日志份数<input v-model.number="server.log_keep_files" type="number" min="1" max="20" required></label>
        <label><span><input v-model="server.lan_access" type="checkbox">允许局域网播放器连接</span></label>
      </div>
      <p class="hint">保存后重启后台。更换主地址会切换缓存目录。</p>
      <div class="toolbar form-actions">
        <button class="primary" :disabled="savingServer || !serverChanged">{{ savingServer ? '保存中…' : '保存服务器' }}</button>
        <p id="server-message" class="hint" role="status">{{ serverMessage || desktop?.last_error }}</p>
      </div>
    </form>

    <form v-if="policy" id="policy-form" class="panel" @submit.prevent="savePolicy">
      <h2>播放与下载</h2>
      <div class="settings">
        <label>播放记录
          <select id="history-mode" v-model="mode">
            <option value="sync">同步记录</option>
            <option value="local">本地记录</option>
            <option value="incognito">无痕模式</option>
          </select>
        </label>
        <label v-for="[key, label, min, max, scale] in downloadFields" :key="key">
          {{ label }}<input v-model.number="numbers[key]" type="number" :min="min" :max="max" :step="scale ? 'any' : 1" required>
        </label>
        <label><span><input v-model="policy.pause_during_playback" type="checkbox" aria-describedby="playback-yield-note">播放回源时暂停下载</span></label>
        <label><span><input v-model="policy.auto_prefetch" type="checkbox">自动预取后续剧集</span></label>
        <label v-if="policy.prefetch_after_seconds !== undefined">
          {{ policy.cleanup_previous_episodes === undefined ? '预取' : '预取与提前清理' }}前播放时长（秒，0 为立即）
          <input v-model.number="policy.prefetch_after_seconds" type="number" min="0" max="3600" step="1" required>
        </label>
        <label v-if="snapshot?.automation.prevent_idle_sleep_supported"><span><input v-model="policy.prevent_idle_sleep" type="checkbox">播放或下载时防止休眠</span></label>
      </div>
      <p id="playback-yield-note" class="hint settings-note">从飞牛读取视频时暂停后台下载。只读本地缓存时，下载照常。</p>
      <p v-if="historyNote" class="hint settings-note">{{ historyNote }}</p>
      <h2 class="settings-group">缓存</h2>
      <div class="settings">
        <label v-for="[key, label, min, max] in cacheFields" :key="key">
          {{ label }}<input v-model.number="numbers[key]" type="number" :min="min" :max="max" step="any" required>
        </label>
        <label v-if="policy.cleanup_previous_episodes !== undefined">
          <span><input v-model="policy.cleanup_previous_episodes" type="checkbox" :disabled="(!optionalRetention && !policy.cleanup_enabled) || mode === 'incognito'" aria-describedby="episode-cleanup-note">看下一集时清理之前的集</span>
        </label>
        <label>
          看完后保留（小时）
          <input
            v-model.number="retentionHours" type="number" min="0" max="8760" step="any"
            :required="!optionalRetention" :placeholder="optionalRetention ? '留空则不定时清理' : undefined"
            aria-describedby="retention-note"
          >
        </label>
      </div>
      <p v-if="policy.cleanup_previous_episodes !== undefined" id="episode-cleanup-note" class="hint settings-note">
        {{ snapshot?.automation.cross_season_cleanup_supported
          ? '同一部剧的后续集播满设定时长后，清理之前各集的缓存，包括之前的季和跳过的集。'
          : '同一季的后续集播满设定时长后，清理之前各集的缓存，包括跳过的集。' }}
        默认 60 秒。
      </p>
      <p id="retention-note" class="hint settings-note">看完这一集就停下时，缓存会在保留期满后清理。关闭提前清理时，所有已看完的集都按这个时间保留。{{ optionalRetention ? '留空则不按时间清理。' : '' }}</p>
      <details>
        <summary>高级</summary>
        <div class="settings">
          <label v-for="[key, label, min, max, scale] in advancedFields" :key="key">
            {{ label }}<input v-model.number="numbers[key]" type="number" :min="min" :max="max" :step="scale ? 'any' : 1" required>
          </label>
        </div>
      </details>
      <div class="toolbar form-actions">
        <button class="primary" :disabled="savingPolicy || !policyChanged">{{ savingPolicy ? '保存中…' : '保存偏好' }}</button>
        <p id="settings-state" class="hint" role="status">{{ policyMessage }}</p>
      </div>
    </form>
  </section>
</template>
