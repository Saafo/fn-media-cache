<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { errorText, jobAction, post } from './api'
import type { Download, DownloadCommand, ManagedMedia } from './api'
import { canControlDownload } from './downloads'
import TaskPopover from './TaskPopover.vue'

const props = defineProps<{
  job: Download
  managed: ManagedMedia[]
  position: number
  queueLength: number
  menuOpen: boolean
  batchBusy: boolean
}>()
const emit = defineEmits<{ refresh: []; notice: [message: string]; toggleMenu: []; closeMenu: [] }>()
const running = ref(false)
const busy = computed(() => running.value || props.batchBusy)
const confirming = ref(false)
const confirmButton = ref<HTMLButtonElement>()
const removeButton = ref<HTMLButtonElement>()
const canPause = computed(() => canControlDownload(props.job.state, 'pause'))
const canResume = computed(() => canControlDownload(props.job.state, 'resume'))
const canCancel = computed(() => canControlDownload(props.job.state, 'cancel'))
const canDelete = computed(() => canControlDownload(props.job.state, 'delete'))
const canManage = computed(() => canDelete.value && props.managed.length > 0)

watch(() => props.menuOpen, open => {
  if (!open) confirming.value = false
})
watch(() => props.job.state, () => {
  emit('closeMenu')
})

async function run(operation: () => Promise<unknown>, message = '') {
  if (busy.value) return
  running.value = true
  emit('notice', '')
  try {
    await operation()
    emit('closeMenu')
    if (message) emit('notice', message)
    emit('refresh')
  } catch (error) {
    emit('notice', errorText(error))
  } finally {
    running.value = false
  }
}

function command(kind: DownloadCommand) {
  return run(() => jobAction(props.job, kind))
}

function resume() {
  return run(async () => {
    if (props.job.state === 'removed') {
      for (const entry of props.managed) {
        const result = await post('/library/download', { session_id: entry.session_id, item_guids: [entry.item.guid] })
        if (result.results.some(row => row.ok)) return
      }
    }
    await jobAction(props.job, 'resume')
  })
}

function retain(entry: ManagedMedia, field: 'keep' | 'completed') {
  const identity = { session_id: entry.session_id, item_guid: entry.item.guid }
  return run(() => post('/managed/retention', {
    ...identity,
    ...(field === 'keep' ? { keep: !entry.keep } : { completed: !entry.completed_at }),
  }))
}

async function askDelete(value: boolean) {
  confirming.value = value
  await nextTick()
  if (value) confirmButton.value?.focus()
  else removeButton.value?.focus()
}

function remove() {
  return run(async () => {
    await jobAction(props.job, 'delete')
    confirming.value = false
    emit('closeMenu')
  }, '已请求删除。')
}
</script>

<template>
  <div class="toolbar">
    <button v-if="canResume" type="button" :disabled="busy" @click="resume">
      {{ job.state === 'removed' ? '重新下载' : '继续' }}
    </button>
    <button v-else-if="canPause" type="button" :disabled="busy" @click="command('pause')">暂停</button>
    <TaskPopover
      v-if="canManage || canCancel || canDelete || position >= 0" :open="menuOpen" label="更多操作"
      trigger-class="task-more" @toggle="emit('toggleMenu')" @close="emit('closeMenu')"
    >
      <template #trigger><span aria-hidden="true">⋯</span></template>
      <div class="task-menu">
        <button v-if="canPause && canResume" type="button" :disabled="busy" @click="command('pause')">暂停</button>
        <template v-if="canManage">
          <template v-for="entry in managed" :key="entry.session_id + '/' + entry.item.guid">
            <button type="button" :disabled="busy" @click="retain(entry, 'keep')">
              {{ entry.keep ? '取消保留' : '保留' }}{{ managed.length > 1 ? ' · ' + (entry.item.title || entry.item.guid) : '' }}
            </button>
            <button type="button" title="只影响本地缓存清理" :disabled="busy" @click="retain(entry, 'completed')">
              {{ entry.completed_at ? '标记未看完' : '标记看完' }}{{ managed.length > 1 ? ' · ' + (entry.item.title || entry.item.guid) : '' }}
            </button>
          </template>
        </template>
        <button v-if="position > 0" type="button" :disabled="busy" @click="command('up')">提前</button>
        <button v-if="position >= 0 && position < queueLength - 1" type="button" :disabled="busy" @click="command('down')">推后</button>
        <button v-if="position > 1" type="button" :disabled="busy" @click="command('first')">移到队首</button>
        <button v-if="canCancel" type="button" :disabled="busy" @click="command('cancel')">取消任务</button>
        <template v-if="canDelete">
          <button v-if="!confirming" ref="removeButton" type="button" class="danger" :disabled="busy" @click="askDelete(true)">删除缓存</button>
          <template v-else>
            <p class="hint">只删除本地文件。</p>
            <button ref="confirmButton" type="button" class="danger" :disabled="busy" @click="remove">确认删除</button>
            <button type="button" :disabled="busy" @click="askDelete(false)">返回</button>
          </template>
        </template>
      </div>
    </TaskPopover>
  </div>
</template>
