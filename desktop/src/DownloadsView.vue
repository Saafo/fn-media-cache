<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { ApiError, jobAction, post } from './api'
import type { Download, ManagedMedia, MediaItem, Snapshot } from './api'
import { canControlDownload, downloadSpeed, duration, reorderStates, size, waitingStates } from './downloads'
import type { DownloadControl } from './downloads'
import DownloadRow from './DownloadRow.vue'

const props = defineProps<{ snapshot?: Snapshot; visible: boolean }>()
const emit = defineEmits<{ refresh: []; notice: [message: string] }>()
const filters = [
  ['all', '全部'], ['downloading', '下载中'], ['waiting', '等待中'],
  ['paused', '已暂停'], ['complete', '已缓存'],
] as const
type Filter = typeof filters[number][0]
const selected = ref<Filter>('all')
const picked = ref(new Set<string>())
const batchBusy = ref(false)
const confirmingDelete = ref(false)
const confirmDeleteButton = ref<HTMLButtonElement>()
const selectionLocked = computed(() => batchBusy.value || confirmingDelete.value)
const openMenu = ref('')
const expanded = ref(new Set<string>())
const seriesBySeason = ref(new Map<string, MediaItem>())
const retrySeriesAt = new Map<string, number>()
let resolvingSeries = false
let supportsSeries = true
const downloads = computed(() => (props.snapshot?.downloads ?? []).filter(job => job.state !== 'removed'))
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
const tasks = computed(() => {
  if (selected.value === 'all') return downloads.value
  if (selected.value === 'waiting') return waiting.value
  return downloads.value.filter(job => job.state === selected.value)
})
const pickedTasks = computed(() => tasks.value.filter(job => picked.value.has(job.id)))
const allPicked = computed(() => tasks.value.length > 0 && pickedTasks.value.length === tasks.value.length)
const commands = [['pause', '暂停'], ['resume', '继续'], ['cancel', '取消任务'], ['delete', '删除缓存']] as const
const batchActions = computed(() => commands.map(([command, label]) => ({
  command, label, jobs: pickedTasks.value.filter(job => canControlDownload(job.state, command)),
})).filter(action => action.jobs.length))
const deletableCount = computed(() => batchActions.value.find(action => action.command === 'delete')?.jobs.length ?? 0)

function chooseFilter(filter: Filter) {
  if (filter === selected.value) return
  picked.value.clear()
  selected.value = filter
}

function pickTask(id: string, value: boolean) {
  if (value) picked.value.add(id)
  else picked.value.delete(id)
}

function pickTasks(jobs: Download[], event: Event) {
  const value = (event.currentTarget as HTMLInputElement).checked
  for (const job of jobs) pickTask(job.id, value)
}

async function runBatch(command: DownloadControl) {
  if (batchBusy.value) return
  const action = batchActions.value.find(action => action.command === command)
  if (!action) return
  openMenu.value = ''
  if (command === 'delete' && !confirmingDelete.value) {
    confirmingDelete.value = true
    await nextTick()
    confirmDeleteButton.value?.focus()
    return
  }
  confirmingDelete.value = false
  batchBusy.value = true
  emit('notice', '')
  let completed = 0
  try {
    for (const job of action.jobs) {
      if (!picked.value.has(job.id)) continue
      try {
        await jobAction(job, command)
        picked.value.delete(job.id)
        completed++
      } catch {
        // Keep unsuccessful tasks selected so the same operation can be retried.
      }
    }
    const result = command === 'delete' ? '已请求删除' : `已${action.label}`
    const incomplete = action.jobs.length - completed
    emit('notice', `${result} ${completed} 项${incomplete ? `，${incomplete} 项未成功` : ''}。`)
  } finally {
    batchBusy.value = false
    emit('refresh')
  }
}

type EpisodeTask = { job: Download; entry: ManagedMedia }
type SeriesGroup = { key: string; title: string; episodes: EpisodeTask[] }

function episodeSelection(episodes: EpisodeTask[]) {
  const checked = episodes.every(({ job }) => picked.value.has(job.id))
  return { checked, indeterminate: !checked && episodes.some(({ job }) => picked.value.has(job.id)) }
}

function seasonKey(entry: ManagedMedia) {
  return `${entry.session_id}/${entry.item.parent_guid}`
}

async function resolveSeries() {
  if (!props.visible || resolvingSeries || !supportsSeries) return
  resolvingSeries = true
  try {
    for (const entry of automation.value?.managed ?? []) {
      if (!props.visible) break
      const item = entry.item
      if (item.kind !== 'Episode' || !item.parent_guid) continue
      const key = seasonKey(entry)
      if (seriesBySeason.value.has(key) || (retrySeriesAt.get(key) ?? 0) > Date.now()) continue
      try {
        const series = await post('/library/series', { session_id: entry.session_id, item_guid: item.parent_guid })
        seriesBySeason.value.set(key, series)
        if (expanded.value.delete(key)) {
          expanded.value.add(`${entry.session_id}/${series.guid}`)
          openMenu.value = ''
        }
      } catch (error) {
        if (error instanceof ApiError && error.status === 404) {
          supportsSeries = false
          break
        }
        retrySeriesAt.set(key, Date.now() + 60_000)
      }
    }
  } finally {
    resolvingSeries = false
  }
}

const groups = computed(() => {
  const groups = new Map<string, SeriesGroup>()
  const rows: (Download | SeriesGroup)[] = []
  for (const job of tasks.value) {
    const entry = managed.value.get(job.id)?.find(({ item }) => item.kind === 'Episode' && item.parent_guid)
    if (!entry) {
      rows.push(job)
      continue
    }
    const season = seasonKey(entry)
    const series = seriesBySeason.value.get(season)
    const key = series ? `${entry.session_id}/${series.guid}` : season
    let group = groups.get(key)
    if (!group) {
      group = { key, title: series?.title || entry.item.tv_title || '', episodes: [] }
      groups.set(key, group)
      rows.push(group)
    }
    group.episodes.push({ job, entry })
  }
  return rows
})

function groupTitle(group: SeriesGroup) {
  if (group.title) return group.title
  const first = group.episodes[0].entry.item
  if (first.title && group.episodes.every(({ entry }) => entry.item.title === first.title)) return first.title
  return `${seasonTitle(first.season_number)} · ${first.title || first.guid}`
}

function seasonTitle(season: number) {
  return season ? `第 ${season} 季` : '未分季'
}

function seasons(group: SeriesGroup) {
  const bySeason = new Map<number, EpisodeTask[]>()
  for (const episode of group.episodes) {
    const number = episode.entry.item.season_number
    const season = bySeason.get(number) ?? []
    season.push(episode)
    bySeason.set(number, season)
  }
  return [...bySeason].sort(([a], [b]) => a - b).map(([number, episodes]) => ({
    number,
    episodes: episodes.sort((a, b) => a.entry.item.episode_number - b.entry.item.episode_number),
  }))
}

function groupSummary(group: SeriesGroup) {
  const states = new Map<string, string>()
  for (const { job, entry } of group.episodes) {
    const current = states.get(entry.item.guid)
    if (!current || job.state === 'complete' || (current !== 'complete' && job.state === 'downloading')) {
      states.set(entry.item.guid, job.state)
    }
  }
  const jobs = [...states.values()]
  const count = (state: string) => jobs.filter(value => value === state).length
  return [
    `${jobs.length} 集`,
    count('complete') && `已缓存 ${count('complete')} 集`,
    count('downloading') && `下载中 ${count('downloading')} 集`,
    count('queued') && `排队 ${count('queued')} 集`,
    count('paused') && `暂停 ${count('paused')} 集`,
  ].filter(Boolean).join(' · ')
}

function toggleSeries(key: string) {
  if (!expanded.value.delete(key)) expanded.value.add(key)
  openMenu.value = ''
}

watch(() => props.visible, visible => {
  if (!visible) {
    openMenu.value = ''
    confirmingDelete.value = false
  }
  else void resolveSeries()
})
watch(automation, () => void resolveSeries(), { immediate: true })
watch(tasks, values => {
  if (openMenu.value && !values.some(job => job.id === openMenu.value.split(':')[1])) openMenu.value = ''
  if (picked.value.size) {
    const visibleIds = new Set(values.map(job => job.id))
    for (const id of picked.value) if (!visibleIds.has(id)) picked.value.delete(id)
  }
})

function closeMenu(id: string) {
  if (openMenu.value === id) openMenu.value = ''
}

const metrics = computed(() => {
  const speed = downloadSpeed(downloads.value)
  return [
    ['下载中 / 等待中', `${active.value.length} / ${waiting.value.length}`],
    ['下载速度', speed ? `${size(speed)}/s` : '—'],
  ]
})
const taskBytes = computed(() => downloads.value.reduce((sum, job) => sum + job.downloaded, 0))
const cacheLimit = computed(() => automation.value?.cache_limit_bytes ?? 0)
const quotaPercent = computed(() => {
  const percent = cacheLimit.value ? 100 * taskBytes.value / cacheLimit.value : 0
  return percent > 0 && percent < 1 ? '<1%' : `${Math.round(percent)}%`
})
const downloadStatus = computed(() => {
  const policy = automation.value?.policy
  if (!policy) return ''
  const yielding = automation.value?.downloads_yielding ?? (automation.value?.playing && policy.pause_during_playback)
  if (yielding) return '播放优先，下载暂缓'
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
  if (job.waiting_reason === 'playback') return '播放优先'
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
        <div class="cache-quota">
          <div class="quota-caption"><span>任务缓存 / 上限</span><span v-if="cacheLimit" class="number">{{ quotaPercent }}</span></div>
          <strong>{{ size(taskBytes) }} / {{ size(cacheLimit || undefined) }}</strong>
          <progress
            v-if="cacheLimit" :max="cacheLimit" :value="Math.min(taskBytes, cacheLimit)"
            :aria-valuetext="`${size(taskBytes)} 已用，共 ${size(cacheLimit)}`" aria-label="任务缓存占用"
            :class="{ 'quota-full': taskBytes >= cacheLimit }"
          ></progress>
        </div>
      </div>
      <div class="table-panel">
        <div class="table-tools page-head">
          <div class="toolbar filters" aria-label="按状态筛选">
            <button
              v-for="[key, label] in filters" :key="key" type="button"
              :aria-pressed="selected === key" :disabled="selectionLocked" @click="chooseFilter(key)"
            >{{ label }}</button>
          </div>
          <p v-if="downloadStatus" class="hint">{{ downloadStatus }}</p>
        </div>
        <div v-if="pickedTasks.length || selectionLocked" class="toolbar table-tools">
          <span role="status">{{ batchBusy ? '正在处理…' : `已选 ${pickedTasks.length} 项` }}</span>
          <template v-if="confirmingDelete">
            <button
              ref="confirmDeleteButton" type="button" class="danger" :disabled="!deletableCount" @click="runBatch('delete')"
            >确认删除 {{ deletableCount }} 项本地缓存</button>
            <button type="button" @click="confirmingDelete = false">返回</button>
          </template>
          <template v-else>
            <button
              v-for="{ command, label, jobs } in batchActions" :key="command"
              type="button" :class="{ danger: command === 'delete' }" :disabled="batchBusy" @click="runBatch(command)"
            >{{ label }}（{{ jobs.length }}）</button>
            <button type="button" :disabled="batchBusy" @click="picked.clear()">清空已选</button>
          </template>
        </div>
        <table class="download-table" aria-label="下载任务">
          <thead><tr>
            <th scope="col" class="task-select">
              <input
                v-if="tasks.length" type="checkbox" aria-label="全选当前筛选结果"
                :checked="allPicked" :indeterminate="!allPicked && pickedTasks.length > 0" :disabled="selectionLocked"
                @change="pickTasks(tasks, $event)"
              >
            </th>
            <th scope="col">影片 / 剧集</th><th scope="col">状态</th><th scope="col">缓存量</th>
            <th scope="col">速度 / 剩余</th><th scope="col" class="align-end">操作</th>
          </tr></thead>
          <tbody id="downloads">
            <template v-for="row in groups" :key="'episodes' in row ? row.key : row.id">
              <template v-if="'episodes' in row">
                <tr class="task-series">
                  <td class="task-select">
                    <input
                      type="checkbox" :aria-label="`选择 ${groupTitle(row)}`" :disabled="selectionLocked"
                      v-bind="episodeSelection(row.episodes)"
                      @change="pickTasks(row.episodes.map(({ job }) => job), $event)"
                    >
                  </td>
                  <th scope="row">
                    <button type="button" class="task-label" :aria-expanded="expanded.has(row.key)" @click="toggleSeries(row.key)">
                      <span aria-hidden="true">{{ expanded.has(row.key) ? '▾' : '▸' }}</span>
                      {{ groupTitle(row) }}
                    </button>
                  </th>
                  <td colspan="4" class="hint">{{ groupSummary(row) }}</td>
                </tr>
                <template v-if="expanded.has(row.key)">
                  <template v-for="season in seasons(row)" :key="season.number">
                    <tr class="task-season">
                      <td class="task-select">
                        <input
                          type="checkbox" :aria-label="`选择 ${groupTitle(row)} ${seasonTitle(season.number)}`"
                          :disabled="selectionLocked" v-bind="episodeSelection(season.episodes)"
                          @change="pickTasks(season.episodes.map(({ job }) => job), $event)"
                        >
                      </td>
                      <th colspan="5" scope="row">{{ seasonTitle(season.number) }}</th>
                    </tr>
                    <DownloadRow
                      v-for="{ job, entry } in season.episodes" :key="job.id"
                      :job="job" :managed="managed.get(job.id) ?? []" :episode-item="entry.item"
                      :series-title="groupTitle(row)" :notes="notes(job)" :description="describe(job)" :transfer="transfer(job)"
                      :badge="badge(job)" :position="reorderIndex.get(job.id) ?? -1" :queue-length="reorderable.length"
                      :selected="picked.has(job.id)" :batch-busy="selectionLocked" @select="pickTask(job.id, $event)"
                      :open-menu="openMenu" @open="openMenu = $event" @close="closeMenu($event)"
                      @refresh="emit('refresh')" @notice="emit('notice', $event)"
                    />
                  </template>
                </template>
              </template>
              <DownloadRow
                v-else :job="row" :managed="managed.get(row.id) ?? []"
                :notes="notes(row)" :description="describe(row)" :transfer="transfer(row)" :badge="badge(row)"
                :position="reorderIndex.get(row.id) ?? -1" :queue-length="reorderable.length"
                :selected="picked.has(row.id)" :batch-busy="selectionLocked" @select="pickTask(row.id, $event)"
                :open-menu="openMenu" @open="openMenu = $event" @close="closeMenu($event)"
                @refresh="emit('refresh')" @notice="emit('notice', $event)"
              />
            </template>
          </tbody>
        </table>
        <p v-if="!tasks.length" class="empty">{{ selected === 'all' ? '暂无下载任务。' : '此状态下没有任务。' }}</p>
      </div>
    </template>
  </section>
</template>
