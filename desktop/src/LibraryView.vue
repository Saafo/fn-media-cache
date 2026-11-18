<script setup lang="ts">
import { computed, ref, shallowRef, watch } from 'vue'
import { errorText, get, post } from './api'
import type { MediaItem, Session, Snapshot } from './api'
import { jobStates, jobTitle, waitingStates } from './downloads'

type CatalogLocation = { mode: 'search' | 'browse'; value: string; page: number; title: string; kind?: string }
const queueBatchSize = 20

const props = defineProps<{ snapshot: Snapshot; visible: boolean }>()
const emit = defineEmits<{ refresh: [] }>()
const sessions = shallowRef<Session[]>([])
const session = ref('')
const query = ref('')
const queryInput = ref<HTMLInputElement>()
const status = ref('')
const selectionStatus = ref('')
const location = ref<CatalogLocation>(initialLocation())
const history = ref<CatalogLocation[]>([])
const items = shallowRef<MediaItem[]>([])
const selected = ref(new Set<string>())
const seasonEpisodes = ref(new Map<string, string[]>())
const selecting = ref(false)
const resultsTable = ref<HTMLTableElement>()
let selectionAnchor = ''
const hasMore = ref(false)
const loading = ref(false)
const submitting = ref(false)
let readingSessions = false
let requestSequence = 0

const sessionNames: Record<string, string> = { needs_login: '请重新登录', offline: '暂不可用', needs_attention: '待验证' }
const locationTitle = (view: CatalogLocation) => view.title || (view.value ? `搜索「${view.value}」` : '媒体库')
const crumbs = computed(() => {
  const trail = [...history.value, location.value]
  if (trail[0].mode === 'search' && trail[0].value) trail.unshift(initialLocation())
  return trail
})
const queue = computed(() => {
  const jobs = props.snapshot.downloads
  return [...jobs.filter(job => job.state === 'downloading'), ...jobs.filter(job => waitingStates.has(job.state))]
})
const queueRows = computed(() => queue.value.slice(0, 4).map(job => ({
  id: job.id,
  title: jobTitle(job, props.snapshot.automation.managed.filter(entry => entry.download_id === job.id)),
  state: jobStates[job.state] + (job.state === 'downloading' && job.total ? ` · ${Math.floor(100 * job.downloaded / job.total)}%` : ''),
})))
const indexedJobs = computed(() => {
  const jobs = new Map(props.snapshot.downloads.map(job => [job.id, job]))
  return new Map(props.snapshot.automation.managed.filter(entry => entry.session_id === session.value)
    .map(entry => [entry.item.guid, jobs.get(entry.download_id)]))
})
const busy = computed(() => loading.value || submitting.value || selecting.value)
const selectableRows = computed(() => items.value.filter(canSelect))
const pageSelected = computed(() => selectableRows.value.length > 0 && selectableRows.value.every(checked))
const pagePartial = computed(() => !pageSelected.value && selectableRows.value.some(item => checked(item) || partial(item)))

function initialLocation(): CatalogLocation {
  return { mode: 'search', value: '', page: 1, title: '' }
}

function changeSession(id: string) {
  if (id === session.value) return
  session.value = id
  requestSequence++
  loading.value = false
  submitting.value = false
  selecting.value = false
  query.value = ''
  location.value = initialLocation()
  history.value = []
  items.value = []
  hasMore.value = false
  selected.value.clear()
  seasonEpisodes.value.clear()
  selectionAnchor = ''
  selectionStatus.value = ''
  if (id) void load(location.value)
  else status.value = '请先让播放器通过代理登录。'
}

async function pollSessions() {
  if (!props.visible || readingSessions) return
  readingSessions = true
  try {
    const next = await get('/sessions')
    if (!props.visible) return
    sessions.value = next
    if (!next.length) {
      changeSession('')
      status.value = '请先让播放器通过代理登录。'
    } else if (!next.some(value => value.id === session.value)) {
      changeSession(next[0].id)
    }
  } catch (error) {
    if (props.visible && !session.value) status.value = errorText(error)
  } finally {
    readingSessions = false
  }
}

watch(() => props.snapshot, pollSessions)
watch(() => props.visible, pollSessions, { immediate: true })

function itemType(item: MediaItem) {
  const types: Record<string, string> = { TV: '剧集', Season: '季', Movie: '电影', Video: '视频', Directory: '目录', MediaDB: '媒体库' }
  if (item.kind !== 'Episode') return types[item.kind] || item.kind
  const episode = item.episode_number ? `第 ${item.episode_number} 集` : '剧集'
  return item.season_number ? `第 ${item.season_number} 季 · ${episode}` : episode
}

function itemStatus(item: MediaItem) {
  const job = indexedJobs.value.get(item.guid)
  if (job && job.state !== 'removed') return jobStates[job.state]
  return item.watched ? '已看' : '—'
}

const playable = (item: MediaItem) => ['Episode', 'Movie', 'Video'].includes(item.kind)

async function load(next: CatalogLocation, nextHistory = history.value) {
  if (!session.value || submitting.value || selecting.value) return
  const request = ++requestSequence
  loading.value = true
  status.value = '读取中…'
  try {
    const base = { session_id: session.value, page: next.page }
    const result = next.mode === 'search'
      ? await post('/library/search', { ...base, query: next.value })
      : await post('/library/browse', { ...base, item_guid: next.value })
    if (request !== requestSequence) return
    history.value = nextHistory
    location.value = next
    if (next.mode === 'search') query.value = next.value
    items.value = result.items
    hasMore.value = result.has_more
    selectionAnchor = ''
    if (next.kind === 'Season' && !result.has_more) {
      seasonEpisodes.value.set(next.value, result.items.filter(item => item.kind === 'Episode').map(item => item.guid))
    }
    status.value = result.items.length ? '' : '没有找到影片。'
  } catch (error) {
    if (request === requestSequence) status.value = errorText(error)
  } finally {
    if (request === requestSequence) loading.value = false
  }
}

function search() {
  selectionStatus.value = ''
  void load({ ...initialLocation(), value: query.value.trim() }, [])
}

function clearSearch() {
  query.value = ''
  search()
  queryInput.value?.focus()
}

function openItem(item: MediaItem) {
  void load(
    { mode: 'browse', value: item.guid, page: 1, title: item.title || item.guid, kind: item.kind },
    [...history.value, location.value],
  )
}

function openCrumb(index: number) {
  void load(crumbs.value[index], crumbs.value.slice(0, index))
}

function canQueue(guid: string) {
  const job = indexedJobs.value.get(guid)
  return !job || ['removed', 'cancelled', 'failed'].includes(job.state)
}

function knownChoices(item: MediaItem) {
  return item.kind === 'Season' ? seasonEpisodes.value.get(item.guid) : [item.guid]
}

function canSelect(item: MediaItem) {
  if (!playable(item) && item.kind !== 'Season') return false
  return knownChoices(item)?.some(guid => canQueue(guid) || selected.value.has(guid)) ?? true
}

function checked(item: MediaItem) {
  const choices = knownChoices(item)?.filter(guid => canQueue(guid) || selected.value.has(guid)) ?? []
  return choices.length > 0 && choices.every(guid => selected.value.has(guid))
}

function partial(item: MediaItem) {
  return !checked(item) && Boolean(knownChoices(item)?.some(guid => selected.value.has(guid)))
}

async function loadSeason(guid: string, sessionId: string) {
  const episodes = new Set<string>()
  let page = 1
  do {
    const result = await post('/library/browse', { session_id: sessionId, item_guid: guid, page })
    for (const item of result.items) {
      if (item.kind === 'Episode') episodes.add(item.guid)
    }
    if (!result.has_more) return [...episodes]
    if (sessionId !== session.value) return []
    page++
  } while (page <= 1000)
  throw new Error('分集列表未读取完整。')
}

async function selectRows(rows: MediaItem[], value: boolean) {
  if (!session.value || busy.value || !rows.length) return
  selecting.value = true
  selectionStatus.value = ''
  const sessionId = session.value
  try {
    for (const item of rows) {
      let choices = knownChoices(item)
      if (!choices) {
        if (!value) continue
        choices = await loadSeason(item.guid, sessionId)
        if (sessionId !== session.value) return
        seasonEpisodes.value.set(item.guid, choices)
      }
      for (const guid of choices) {
        if (!value) selected.value.delete(guid)
        else if (canQueue(guid)) selected.value.add(guid)
      }
    }
  } catch (error) {
    if (sessionId === session.value) selectionStatus.value = errorText(error)
  } finally {
    if (sessionId === session.value) selecting.value = false
  }
}

function selectRange(item: MediaItem, extend: boolean) {
  if (!extend || !selectionAnchor) {
    selectionAnchor = item.guid
    void selectRows([item], !checked(item))
    return
  }
  const rows = selectableRows.value
  const start = rows.findIndex(row => row.guid === selectionAnchor)
  const end = rows.findIndex(row => row.guid === item.guid)
  if (start < 0 || end < 0) {
    selectionAnchor = item.guid
    void selectRows([item], true)
    return
  }
  void selectRows(rows.slice(Math.min(start, end), Math.max(start, end) + 1), true)
}

function listKeys(event: KeyboardEvent) {
  const input = event.target
  if (busy.value) return
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'a') {
    event.preventDefault()
    void selectRows(selectableRows.value, true)
    return
  }
  if (!(input instanceof HTMLInputElement) || input.type !== 'checkbox') return
  const rows = selectableRows.value
  const index = rows.findIndex(item => item.guid === input.dataset.guid)
  if (index < 0) return
  if (event.shiftKey && event.key === ' ') {
    event.preventDefault()
    selectRange(rows[index], true)
    return
  }
  const step = event.key === 'ArrowDown' ? 1 : event.key === 'ArrowUp' ? -1 : 0
  if (!step || !rows[index + step]) return
  event.preventDefault()
  const next = rows[index + step]
  const inputs = resultsTable.value?.querySelectorAll<HTMLInputElement>('input[data-guid]')
  Array.from(inputs ?? []).find(element => element.dataset.guid === next.guid)?.focus()
  if (event.shiftKey) {
    selectionAnchor ||= rows[index].guid
    selectRange(next, true)
  }
}

function clearSelection() {
  selected.value.clear()
  selectionAnchor = ''
  selectionStatus.value = ''
}

async function enqueue() {
  if (busy.value || !selected.value.size) return
  submitting.value = true
  selectionStatus.value = ''
  const request = requestSequence
  const guids = [...selected.value]
  const sessionId = session.value
  let added = 0
  let skipped = 0
  let failed = 0
  let interrupted = false
  try {
    for (let offset = 0; offset < guids.length; offset += queueBatchSize) {
      const batch = guids.slice(offset, offset + queueBatchSize).filter(guid => {
        if (canQueue(guid)) return true
        selected.value.delete(guid)
        skipped++
        return false
      })
      if (!batch.length) continue
      const result = await post('/library/download', { session_id: sessionId, item_guids: batch })
      if (request !== requestSequence) return
      const successful = new Set(result.results.filter(row => row.ok).map(row => row.item_guid))
      for (const guid of batch) {
        if (!successful.has(guid)) {
          failed++
          continue
        }
        selected.value.delete(guid)
        added++
      }
      emit('refresh')
    }
  } catch {
    interrupted = true
  } finally {
    if (request === requestSequence) {
      submitting.value = false
      emit('refresh')
      selectionStatus.value = [
        added ? `已入队 ${added} 项。` : '',
        skipped ? `跳过 ${skipped} 项已有任务。` : '',
        failed ? `${failed} 项入队失败。` : '',
        interrupted ? '入队中断，可重试剩余项。' : '',
      ].join(' ')
    }
  }
}
</script>

<template>
  <section data-view="library">
    <header class="page-head"><h1 tabindex="-1">媒体库</h1></header>
    <form id="catalog-search" class="toolbar catalog-search" @submit.prevent="search">
      <select
        v-if="sessions.length > 1" aria-label="播放会话" :value="session" :disabled="busy"
        @change="changeSession(($event.currentTarget as HTMLSelectElement).value)"
      >
        <option v-for="entry in sessions" :key="entry.id" :value="entry.id">
          {{ entry.origin }} · {{ sessionNames[entry.status] || entry.id.slice(0, 6) }}
        </option>
      </select>
      <input id="search-query" ref="queryInput" v-model="query" aria-label="片名" placeholder="搜索片名" maxlength="200" :disabled="!session || submitting || selecting">
      <button
        v-if="query || (location.mode === 'search' && location.value)"
        type="button" aria-label="清除搜索" :disabled="!session || busy" @click="clearSearch"
      >清除</button>
      <button :disabled="!session || busy">搜索</button>
    </form>
    <div class="catalog-layout">
      <div class="table-panel catalog-results">
        <header class="panel-head catalog-header">
          <nav id="catalog-title" aria-label="媒体库路径">
            <ol class="catalog-crumbs">
              <li v-for="(crumb, index) in crumbs" :key="`${crumb.mode}:${crumb.value}`">
                <span v-if="index" aria-hidden="true">›</span>
                <span v-if="index === crumbs.length - 1" aria-current="page">{{ locationTitle(crumb) }}</span>
                <button v-else type="button" :disabled="busy" @click="openCrumb(index)">{{ locationTitle(crumb) }}</button>
              </li>
            </ol>
          </nav>
        </header>
        <p v-if="status" class="empty" role="status">{{ status }}</p>
        <div v-if="items.length" class="catalog-scroll">
          <table ref="resultsTable" aria-label="媒体库列表" tabindex="0" @keydown="listKeys">
            <thead><tr>
              <th scope="col" class="catalog-select">
                <input
                  v-if="selectableRows.length" type="checkbox" aria-label="全选当前页" title="全选当前页（⌘A / Ctrl+A）"
                  :checked="pageSelected" :indeterminate="pagePartial" :aria-disabled="busy"
                  @click.prevent="selectRows(selectableRows, !pageSelected)"
                >
              </th>
              <th scope="col">名称</th><th scope="col">集数 / 类型</th><th scope="col">状态</th>
            </tr></thead>
            <tbody id="catalog">
              <tr v-for="item in items" :key="item.guid" :class="{ 'catalog-picked': checked(item) || partial(item) }">
                <td>
                  <input
                    v-if="playable(item) || item.kind === 'Season'" :id="'pick-' + item.guid" type="checkbox"
                    :aria-label="(item.kind === 'Season' ? '选择整季 ' : '选择 ') + (item.title || item.guid)" :data-guid="item.guid"
                    :checked="checked(item)" :indeterminate="partial(item)" :disabled="!canSelect(item)" :aria-disabled="busy"
                    @click.prevent="selectRange(item, $event.shiftKey)"
                  >
                </td>
                <td>
                  <label v-if="playable(item)" class="title" :for="'pick-' + item.guid">{{ item.title || item.guid }}</label>
                  <button
                    v-else type="button" :disabled="busy" @click="openItem(item)"
                  >{{ item.title || item.guid }}</button>
                </td>
                <td>{{ itemType(item) }}</td><td>{{ itemStatus(item) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <div
          v-if="selectableRows.length || selected.size || selecting || selectionStatus || location.page > 1 || hasMore"
          class="toolbar catalog-footer" :class="{ 'selection-bar': selected.size || selectionStatus }"
        >
          <button v-if="location.page > 1" type="button" :disabled="busy" @click="load({ ...location, page: location.page - 1 })">上一页</button>
          <small v-if="location.page > 1 || hasMore">第 {{ location.page }} 页</small>
          <button v-if="hasMore" type="button" :disabled="busy" @click="load({ ...location, page: location.page + 1 })">下一页</button>
          <p v-if="selecting || selectionStatus" id="catalog-status" class="hint" role="status">{{ selecting ? '正在读取分集…' : selectionStatus }}</p>
          <button v-if="selected.size" type="button" :disabled="busy" @click="clearSelection">清空已选</button>
          <button
            v-if="selectableRows.length || selected.size" id="download-selection" type="button" class="primary"
            :disabled="!selected.size || busy" @click="enqueue"
          >{{ submitting ? '正在入队…' : selected.size ? `加入队列（${selected.size}）` : '加入队列' }}</button>
        </div>
      </div>
      <aside class="panel">
        <h2>下载队列</h2>
        <p v-if="!queueRows.length" class="hint">队列为空。</p>
        <ul v-else id="queue-preview" class="queue-preview">
          <li v-for="job in queueRows" :key="job.id"><span class="title">{{ job.title }}</span><small>{{ job.state }}</small></li>
          <li v-if="queue.length > queueRows.length" class="hint">另有 {{ queue.length - queueRows.length }} 项</li>
        </ul>
      </aside>
    </div>
  </section>
</template>
