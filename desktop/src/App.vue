<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, shallowRef } from 'vue'
import { ApiError, desktopCall, errorText, get, isDesktop, storeToken } from './api'
import type { DesktopStatus, Snapshot } from './api'
import DownloadsView from './DownloadsView.vue'
import OverviewView from './OverviewView.vue'
import LibraryView from './LibraryView.vue'
import SettingsView from './SettingsView.vue'

const pages = [
  ['overview', '运行总览'],
  ['tasks', '下载任务'],
  ['library', '媒体库'],
  ['settings', '设置'],
] as const
type Page = typeof pages[number][0]

const page = ref<Page>('overview')
const tokenInput = ref('')
const message = ref('')
const connectionError = ref('')
const main = ref<HTMLElement>()
const snapshot = shallowRef<Snapshot>()
const desktop = shallowRef<DesktopStatus>()
let authenticated = false
let polling = false
let refreshPending = false
let pollingDesktop = false
let generation = 0
let pollTimer = 0
let desktopTimer = 0

const serviceLabel = computed(() => {
  if (!isDesktop) return snapshot.value ? '已连接' : '待连接'
  if (!desktop.value) return '后台不可用'
  if (desktop.value.running) return '代理运行中'
  return desktop.value.configured ? '后台已停止' : '待配置'
})
const address = computed(() => desktop.value?.running ? desktop.value.proxy_address.replace(/^https?:\/\//, '') : '')

function report(text = '') {
  message.value = text
}

async function choose(next: Page) {
  if (next === page.value) return
  page.value = next
  report()
  await nextTick()
  if (next !== page.value) return
  window.scrollTo(0, 0)
  main.value?.querySelector<HTMLElement>(`[data-view="${next}"] h1`)?.focus({ preventScroll: true })
}

function invalidate() {
  generation++
  snapshot.value = undefined
}

async function refresh(afterAction = false) {
  if (isDesktop ? !desktop.value?.running : !authenticated) return
  if (polling) {
    if (afterAction) refreshPending = true
    return
  }
  polling = true
  const current = generation
  try {
    const [stats, downloads, automation] = await Promise.all([get('/stats'), get('/downloads'), get('/automation')])
    if (current !== generation) return
    snapshot.value = { stats, downloads, automation }
    connectionError.value = ''
  } catch (error) {
    if (current !== generation) return
    if (!isDesktop && error instanceof ApiError && error.status === 401) {
      authenticated = false
      storeToken('')
      invalidate()
    }
    connectionError.value = errorText(error)
  } finally {
    polling = false
    const refreshAgain = current !== generation || refreshPending
    refreshPending = false
    if (refreshAgain) void refresh()
  }
}

function connect() {
  const value = tokenInput.value.trim()
  if (!value) return
  tokenInput.value = ''
  storeToken(value)
  authenticated = true
  report()
  connectionError.value = ''
  invalidate()
  void refresh()
}

function applyDesktop(status: DesktopStatus | undefined, restarted = false) {
  const firstStatus = !desktop.value
  if (Boolean(desktop.value?.running) !== Boolean(status?.running) || restarted) invalidate()
  desktop.value = status
  if (firstStatus && status && !status.configured && page.value === 'overview') choose('settings')
  if (status?.running) void refresh()
}

async function updateDesktop() {
  if (pollingDesktop) return
  pollingDesktop = true
  const current = generation
  try {
    const status = await desktopCall('service_status', undefined)
    if (current === generation) applyDesktop(status)
  } catch {
    if (current === generation) applyDesktop(undefined)
  } finally {
    pollingDesktop = false
  }
}

onMounted(() => {
  pollTimer = window.setInterval(refresh, 1500)
  if (isDesktop) {
    void updateDesktop()
    desktopTimer = window.setInterval(updateDesktop, 3000)
  }
})
onUnmounted(() => {
  window.clearInterval(pollTimer)
  window.clearInterval(desktopTimer)
})
</script>

<template>
  <div class="shell">
    <aside class="sidebar">
      <div class="brand">FN Media Cache</div>
      <nav aria-label="主导航">
        <button
          v-for="[key, label] in pages"
          :key="key"
          type="button"
          :aria-current="page === key ? 'page' : undefined"
          @click="choose(key)"
        >{{ label }}</button>
      </nav>
      <div class="service-state">
        <strong id="service-state" :data-running="Boolean(snapshot || desktop?.running)">{{ serviceLabel }}</strong>
        <code v-if="address">{{ address }}</code>
      </div>
    </aside>
    <main ref="main">
      <form v-if="!isDesktop && !snapshot" id="login" class="panel login" @submit.prevent="connect">
        <label for="token"><strong>管理令牌</strong></label>
        <div class="toolbar">
          <input id="token" v-model="tokenInput" type="password" autocomplete="off" required>
          <button class="primary">连接</button>
        </div>
        <p class="hint">令牌在数据目录的 <code>admin-token</code> 文件中。</p>
      </form>
      <p id="message" class="notice" role="status">{{ connectionError || message }}</p>

      <DownloadsView
        v-show="page === 'tasks'" :visible="page === 'tasks'" :snapshot="snapshot"
        @refresh="refresh(true)" @notice="report"
      />
      <OverviewView
        v-show="page === 'overview'" :visible="page === 'overview'" :snapshot="snapshot"
        :can-open-logs="isDesktop && Boolean(desktop?.configured)" @refresh="refresh(true)" @notice="report"
        @show-tasks="choose('tasks')"
      />
      <LibraryView
        v-if="snapshot" v-show="page === 'library'" :visible="page === 'library'" :snapshot="snapshot"
        @refresh="refresh(true)"
      />
      <section v-if="!snapshot && page === 'library'" data-view="library">
        <header class="page-head"><h1 tabindex="-1">媒体库</h1></header>
        <p v-if="isDesktop" class="panel hint">连接后台后可浏览媒体库。</p>
      </section>
      <SettingsView
        v-show="page === 'settings'" :desktop="desktop" :snapshot="snapshot"
        @refresh="refresh(true)" @server-saved="applyDesktop($event, true)"
      />
    </main>
  </div>
</template>
