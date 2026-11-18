<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import type { Download, ManagedMedia, Snapshot } from './api'
import { duration, jobStates, jobTitle, reorderStates, size, waitingStates } from './downloads'
import DownloadActions from './DownloadActions.vue'

const props = defineProps<{ snapshot?: Snapshot; visible: boolean }>()
const emit = defineEmits<{ refresh: []; notice: [message: string] }>()
const filters = [
  ['all', '全部'], ['downloading', '下载中'], ['waiting', '等待中'],
  ['paused', '已暂停'], ['complete', '已缓存'],
] as const
type Filter = typeof filters[number][0]
const selected = ref<Filter>('all')
const table = ref<HTMLTableElement>()
const openMenu = ref('')
const downloads = computed(() => props.snapshot?.downloads ?? [])
const automation = computed(() => props.snapshot?.automation)
const active = computed(() => downloads.value.filter(job => job.state === 'downloading'))
const waiting = computed(() => downloads.value.filter(job => waitingStates.has(job.state)))
const reorderable = computed(() => downloads.value.filter(job => reorderStates.has(job.state)))
const reorderIndex = computed(() => new Map(reorderable.value.map((job, index) => [job.id, index])))
const queueOrder = computed(() => new Map(waiting.value.filter(job => job.state === 'queued').map((job, index) => [job.id, index + 1])))
const managed = computed(() => {
  const entries = new Map<string, ManagedMedia[]>()
  for (const item of automation.value?.managed ?? []) {
    const group = entries.get(item.download_id) ?? []
    group.push(item)
    entries.set(item.download_id, group)
  }
  return entries
})
const rows = computed(() => {
  if (selected.value === 'all') return downloads.value
  if (selected.value === 'waiting') return waiting.value
  return downloads.value.filter(job => job.state === selected.value)
})
watch(() => props.visible, visible => {
  if (!visible) openMenu.value = ''
})
watch(rows, values => {
  if (openMenu.value && !values.some(job => job.id === openMenu.value)) openMenu.value = ''
})

function closeMenu(id: string) {
  if (openMenu.value === id) openMenu.value = ''
}

function dismissOnPointer(event: PointerEvent) {
  if (!openMenu.value || !(event.target instanceof Element)) return
  if (event.target.closest('[data-download-menu]')?.getAttribute('data-download-menu') !== openMenu.value) {
    openMenu.value = ''
  }
}

async function dismissOnEscape(event: KeyboardEvent) {
  if (event.key !== 'Escape' || !openMenu.value) return
  event.preventDefault()
  const summary = table.value?.querySelector<HTMLElement>('details[open] summary')
  openMenu.value = ''
  await nextTick()
  summary?.focus()
}

onMounted(() => {
  document.addEventListener('pointerdown', dismissOnPointer)
  document.addEventListener('keydown', dismissOnEscape)
})
onUnmounted(() => {
  document.removeEventListener('pointerdown', dismissOnPointer)
  document.removeEventListener('keydown', dismissOnEscape)
})
const metrics = computed(() => {
  const startedBytes = downloads.value.reduce((sum, job) => sum + (job.state === 'removed' ? 0 : job.downloaded), 0)
  const speed = active.value.reduce((sum, job) => sum + job.bytes_per_second, 0)
  return [
    ['下载中 / 等待中', `${active.value.length} / ${waiting.value.length}`],
    ['下载速度', speed ? `${size(speed)}/s` : '—'],
    ['任务缓存 / 上限', `${size(startedBytes)} / ${size(automation.value?.cache_limit_bytes)}`],
  ]
})
const downloadStatus = computed(() => {
  const policy = automation.value?.policy
  if (!policy) return ''
  if (automation.value?.playing && policy.pause_during_playback) return '播放时暂停下载'
  return policy.bytes_per_second ? `限速 ${size(policy.bytes_per_second)}/s` : ''
})

function notes(job: Download) {
  const policy = automation.value?.policy
  return (managed.value.get(job.id) ?? []).flatMap(entry => {
    if (entry.keep) return ['已保留']
    if (!entry.completed_at || !policy?.cleanup_enabled || policy.local_history) return []
    return [`清理时间 ${new Date((entry.completed_at + policy.retention_seconds) * 1000).toLocaleString()}`]
  }).join(' · ')
}

function describe(job: Download) {
  if (job.waiting_reason === 'playback') return '播放时暂停'
  if (job.retry_at) return `重试 ${new Date(job.retry_at * 1000).toLocaleTimeString()}`
  return job.error || ''
}

function transfer(job: Download) {
  if (job.state === 'downloading' && job.bytes_per_second) {
    return [`${size(job.bytes_per_second)}/s`, duration(job.eta_seconds)].filter(Boolean).join(' · ')
  }
  const place = queueOrder.value.get(job.id)
  return place ? `第 ${place} 位` : '—'
}

function badge(job: Download) {
  if (job.state === 'downloading') return 'active'
  if (job.state === 'complete') return 'complete'
  if (['failed', 'paused', 'waiting_login', 'waiting_space'].includes(job.state)) return 'warning'
  return ''
}
</script>

<template>
  <section data-view="tasks">
    <header class="page-head"><h1 tabindex="-1">下载任务</h1></header>
    <template v-if="snapshot">
      <div class="panel stats" aria-label="下载与缓存">
        <div v-for="[label, value] in metrics" :key="label"><span>{{ label }}</span><strong>{{ value }}</strong></div>
      </div>
      <div class="table-panel">
        <div class="table-tools page-head">
          <div class="toolbar filters" aria-label="按状态筛选">
            <button
              v-for="[key, label] in filters" :key="key" type="button"
              :aria-pressed="selected === key" @click="selected = key"
            >{{ label }}</button>
          </div>
          <p v-if="downloadStatus" class="hint">{{ downloadStatus }}</p>
        </div>
        <table ref="table" class="download-table" aria-label="下载任务">
          <thead><tr>
            <th scope="col">影片 / 剧集</th><th scope="col">状态</th><th scope="col">进度</th>
            <th scope="col">速度 / 剩余</th><th scope="col" class="align-end">操作</th>
          </tr></thead>
          <tbody id="downloads">
            <tr v-for="job in rows" :key="job.id" :data-id="job.id">
              <td class="path">
                <strong class="title">{{ jobTitle(job, managed.get(job.id) ?? []) }}</strong>
                <small v-if="notes(job)">{{ notes(job) }}</small>
              </td>
              <td><span class="badge" :class="badge(job)">{{ jobStates[job.state] }}</span><p class="hint">{{ describe(job) }}</p></td>
              <td class="task-progress">
                <span class="hint number">{{ job.total ? size(job.downloaded) + ' / ' + size(job.total) : job.downloaded ? size(job.downloaded) : '—' }}</span>
                <progress v-if="job.total" :max="job.total" :value="Math.min(job.downloaded, job.total)" aria-label="下载进度"></progress>
              </td>
              <td>{{ transfer(job) }}</td>
              <td class="task-actions">
                <DownloadActions
                  :job="job" :managed="managed.get(job.id) ?? []"
                  :position="reorderIndex.get(job.id) ?? -1" :queue-length="reorderable.length"
                  :menu-open="openMenu === job.id"
                  @toggle-menu="openMenu = openMenu === job.id ? '' : job.id"
                  @close-menu="closeMenu(job.id)"
                  @refresh="emit('refresh')" @notice="emit('notice', $event)"
                />
              </td>
            </tr>
          </tbody>
        </table>
        <p v-if="!rows.length" class="empty">{{ selected === 'all' ? '暂无下载任务。' : '此状态下没有任务。' }}</p>
      </div>
    </template>
  </section>
</template>
