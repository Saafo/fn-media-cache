<script setup lang="ts">
import { computed, ref, shallowRef, watch } from 'vue'
import { errorText, get, post } from './api'
import type { MediaItem, Session, Snapshot } from './api'
import { jobStates, jobTitle, waitingStates } from './downloads'

type CatalogLocation = { mode: 'search' | 'browse'; value: string; page: number; title: string }

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
const hasMore = ref(false)
const loading = ref(false)
const submitting = ref(false)
let readingSessions = false
let requestSequence = 0

const sessionNames: Record<string, string> = { needs_login: '请重新登录', offline: '暂不可用', needs_attention: '待验证' }
const title = computed(() => [...history.value, location.value].map(view => view.title).filter(Boolean).join(' / ')
  || (location.value.value ? '搜索结果' : '全部'))
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

function initialLocation(): CatalogLocation {
  return { mode: 'search', value: '', page: 1, title: '' }
}

function changeSession(id: string) {
  if (id === session.value) return
  session.value = id
  requestSequence++
  loading.value = false
  submitting.value = false
  query.value = ''
  location.value = initialLocation()
  history.value = []
  items.value = []
  hasMore.value = false
  selected.value.clear()
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

async function load(next: CatalogLocation, navigation: 'replace' | 'open' | 'back' | 'search' = 'replace') {
  if (!session.value || submitting.value) return
  const request = ++requestSequence
  loading.value = true
  status.value = '读取中…'
  try {
    const base = { session_id: session.value, page: next.page }
    const result = next.mode === 'search'
      ? await post('/library/search', { ...base, query: next.value })
      : await post('/library/browse', { ...base, item_guid: next.value })
    if (request !== requestSequence) return
    if (navigation === 'open') history.value.push(location.value)
    if (navigation === 'back') history.value.pop()
    if (navigation === 'search') history.value = []
    location.value = next
    items.value = result.items
    hasMore.value = result.has_more
    status.value = result.items.length ? '' : '没有找到影片。'
  } catch (error) {
    if (request === requestSequence) status.value = errorText(error)
  } finally {
    if (request === requestSequence) loading.value = false
  }
}

function search() {
  selectionStatus.value = ''
  void load({ ...initialLocation(), value: query.value.trim() }, 'search')
}

function clearSearch() {
  query.value = ''
  search()
  queryInput.value?.focus()
}

function back() {
  const previous = history.value[history.value.length - 1]
  if (previous) void load(previous, 'back')
}

function toggleSelection(guid: string, event: Event) {
  const checkbox = event.currentTarget as HTMLInputElement
  if (checkbox.checked && selected.value.size >= 20) {
    checkbox.checked = false
    selectionStatus.value = '每次最多选 20 项。'
    return
  }
  selectionStatus.value = ''
  if (checkbox.checked) selected.value.add(guid)
  else selected.value.delete(guid)
}

function clearSelection() {
  selected.value.clear()
  selectionStatus.value = ''
}

async function enqueue() {
  if (submitting.value || !selected.value.size) return
  submitting.value = true
  selectionStatus.value = ''
  const request = requestSequence
  try {
    const result = await post('/library/download', { session_id: session.value, item_guids: [...selected.value] })
    if (request !== requestSequence) return
    const successful = result.results.filter(row => row.ok)
    for (const item of successful) selected.value.delete(item.item_guid)
    const failed = result.results.length - successful.length
    selectionStatus.value = [
      successful.length ? `已加入 ${successful.length} 项。` : '',
      failed ? `${failed} 项暂时无法下载。` : '',
    ].join('')
    emit('refresh')
  } catch (error) {
    if (request === requestSequence) selectionStatus.value = errorText(error)
  } finally {
    if (request === requestSequence) submitting.value = false
  }
}
</script>

<template>
  <section data-view="library">
    <header class="page-head"><h1 tabindex="-1">媒体库</h1></header>
    <form id="catalog-search" class="toolbar catalog-search" @submit.prevent="search">
      <select
        v-if="sessions.length > 1" aria-label="播放会话" :value="session" :disabled="submitting"
        @change="changeSession(($event.currentTarget as HTMLSelectElement).value)"
      >
        <option v-for="entry in sessions" :key="entry.id" :value="entry.id">
          {{ entry.origin }} · {{ sessionNames[entry.status] || entry.id.slice(0, 6) }}
        </option>
      </select>
      <input id="search-query" ref="queryInput" v-model="query" aria-label="片名" placeholder="搜索片名" maxlength="200" :disabled="!session || submitting">
      <button
        v-if="query || (location.mode === 'search' && location.value)"
        type="button" aria-label="清除搜索" :disabled="!session || submitting" @click="clearSearch"
      >清除</button>
      <button :disabled="!session || loading || submitting">搜索</button>
    </form>
    <div class="catalog-layout">
      <div class="table-panel catalog-results">
        <header class="panel-head catalog-header">
          <h2 id="catalog-title">{{ title }}</h2>
          <button v-if="history.length" id="catalog-back" type="button" :disabled="loading || submitting" @click="back">返回上一级</button>
        </header>
        <p v-if="status" class="empty" role="status">{{ status }}</p>
        <div v-if="items.length" class="catalog-scroll">
          <table aria-label="媒体库">
            <thead><tr><th scope="col" class="catalog-select">选择</th><th scope="col">名称</th><th scope="col">集数 / 类型</th><th scope="col">状态</th></tr></thead>
            <tbody id="catalog">
              <tr v-for="item in items" :key="item.guid">
                <td>
                  <input
                    v-if="playable(item)" :id="'pick-' + item.guid" type="checkbox" :aria-label="'选择' + (item.title || item.guid)" :data-guid="item.guid"
                    :checked="selected.has(item.guid)" :disabled="loading || submitting" @change="toggleSelection(item.guid, $event)"
                  >
                </td>
                <td>
                  <label v-if="playable(item)" class="title" :for="'pick-' + item.guid">{{ item.title || item.guid }}</label>
                  <button
                    v-else type="button" :disabled="loading || submitting"
                    @click="load({ mode: 'browse', value: item.guid, page: 1, title: item.title || item.guid }, 'open')"
                  >{{ item.title || item.guid }}</button>
                </td>
                <td>{{ itemType(item) }}</td><td>{{ itemStatus(item) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <div
          v-if="items.some(playable) || selected.size || selectionStatus || location.page > 1 || hasMore"
          class="toolbar catalog-footer" :class="{ 'selection-bar': selected.size || selectionStatus }"
        >
          <button v-if="location.page > 1" type="button" :disabled="loading || submitting" @click="load({ ...location, page: location.page - 1 })">上一页</button>
          <small v-if="location.page > 1 || hasMore">第 {{ location.page }} 页</small>
          <button v-if="hasMore" type="button" :disabled="loading || submitting" @click="load({ ...location, page: location.page + 1 })">下一页</button>
          <p v-if="selectionStatus" id="catalog-status" class="hint" role="status">{{ selectionStatus }}</p>
          <button v-if="selected.size" type="button" :disabled="submitting" @click="clearSelection">清空已选</button>
          <button
            v-if="items.some(playable) || selected.size" id="download-selection" type="button" class="primary"
            :disabled="!selected.size || loading || submitting" @click="enqueue"
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
