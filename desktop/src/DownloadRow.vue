<script setup lang="ts">
import { computed } from 'vue'
import type { Download, ManagedMedia, MediaItem } from './api'
import { episodeNumber, jobStates, jobTitle, size } from './downloads'
import DownloadActions from './DownloadActions.vue'
import TaskPopover from './TaskPopover.vue'

const props = defineProps<{
  job: Download
  managed: ManagedMedia[]
  episodeItem?: MediaItem
  seriesTitle?: string
  notes: string
  description: string
  transfer: string
  badge: string
  position: number
  queueLength: number
  openMenu: string
  selected: boolean
  batchBusy: boolean
}>()
const emit = defineEmits<{ refresh: []; notice: [message: string]; open: [key: string]; close: [key: string]; select: [value: boolean] }>()
const number = computed(() => episodeNumber(props.episodeItem))
const title = computed(() => {
  const item = props.episodeItem
  return item ? (item.title === props.seriesTitle || item.title === item.tv_title ? '' : item.title) : jobTitle(props.job, props.managed)
})
const label = computed(() => [number.value, title.value].filter(Boolean).join(' · ') || props.job.path)
const amount = computed(() => {
  const { total, downloaded, state } = props.job
  if (state === 'complete') return size(total ?? downloaded)
  if (total) return `${size(downloaded)} / ${size(total)}`
  return downloaded ? size(downloaded) : '—'
})
const detailKey = computed(() => `details:${props.job.id}`)
const actionKey = computed(() => `actions:${props.job.id}`)

function toggle(key: string) {
  if (props.openMenu === key) emit('close', key)
  else emit('open', key)
}
</script>

<template>
  <tr :data-id="job.id" :class="{ 'task-episode': episodeItem }">
    <td class="task-select">
      <input
        type="checkbox" :aria-label="`选择 ${label}`" :checked="selected" :disabled="batchBusy"
        @change="emit('select', ($event.currentTarget as HTMLInputElement).checked)"
      >
    </td>
    <td class="path">
      <TaskPopover
        v-if="notes || description" :open="openMenu === detailKey" :label="`${label}的任务详情`" trigger-class="task-label"
        @toggle="toggle(detailKey)" @close="emit('close', detailKey)"
      >
        <template #trigger>{{ label }}</template>
        <div class="task-details">
          <strong>{{ label }}</strong>
          <p v-if="description">{{ description }}</p>
          <p v-if="notes">{{ notes }}</p>
        </div>
      </TaskPopover>
      <strong v-else class="title">{{ label }}</strong>
    </td>
    <td><span class="badge" :class="badge">{{ jobStates[job.state] }}</span></td>
    <td class="task-progress">
      <span class="hint number">{{ amount }}</span>
      <progress v-if="job.total && job.state !== 'complete'" :max="job.total" :value="Math.min(job.downloaded, job.total)" aria-label="下载进度"></progress>
    </td>
    <td>{{ transfer }}</td>
    <td class="task-actions">
      <DownloadActions
        :job="job" :managed="managed" :position="position" :queue-length="queueLength" :batch-busy="batchBusy"
        :menu-open="openMenu === actionKey" @toggle-menu="toggle(actionKey)" @close-menu="emit('close', actionKey)"
        @refresh="emit('refresh')" @notice="emit('notice', $event)"
      />
    </td>
  </tr>
</template>
