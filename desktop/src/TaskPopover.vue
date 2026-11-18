<script setup lang="ts">
import { onUnmounted, ref, useId, watch } from 'vue'

const props = defineProps<{ open: boolean; label: string; triggerClass?: string }>()
const emit = defineEmits<{ toggle: []; close: [] }>()
const trigger = ref<HTMLButtonElement>()
const panel = ref<HTMLElement>()
const id = useId()
let resize: ResizeObserver | undefined

function place() {
  if (!trigger.value || !panel.value) return
  const anchor = trigger.value.getBoundingClientRect()
  const width = panel.value.offsetWidth
  const height = panel.value.offsetHeight
  const left = Math.max(8, Math.min(window.innerWidth - width - 8, anchor.right - width))
  const top = anchor.bottom + height + 14 <= window.innerHeight
    ? anchor.bottom + 6
    : Math.max(8, anchor.top - height - 6)
  panel.value.style.left = `${left}px`
  panel.value.style.top = `${top}px`
}

function outside(event: PointerEvent) {
  if (event.target instanceof Node && !panel.value?.contains(event.target) && !trigger.value?.contains(event.target)) {
    emit('close')
  }
}

function escape(event: KeyboardEvent) {
  if (event.key !== 'Escape') return
  event.preventDefault()
  emit('close')
  trigger.value?.focus({ preventScroll: true })
}

function unbind() {
  resize?.disconnect()
  document.removeEventListener('pointerdown', outside)
  document.removeEventListener('keydown', escape)
  window.removeEventListener('scroll', place, true)
  window.removeEventListener('resize', place)
}

watch(() => props.open, open => {
  if (!open || !panel.value) {
    unbind()
    return
  }
  place()
  resize = new ResizeObserver(place)
  resize.observe(panel.value)
  document.addEventListener('pointerdown', outside)
  document.addEventListener('keydown', escape)
  window.addEventListener('scroll', place, true)
  window.addEventListener('resize', place)
  const focusTarget = panel.value.querySelector<HTMLButtonElement>('button:not(:disabled)') ?? panel.value
  focusTarget.focus({ preventScroll: true })
}, { flush: 'post' })
onUnmounted(unbind)
</script>

<template>
  <button
    ref="trigger" type="button" :class="triggerClass" :aria-label="label" :aria-expanded="open"
    :aria-controls="open ? id : undefined" @click="emit('toggle')"
  ><slot name="trigger" /></button>
  <Teleport to="body">
    <div v-if="open" :id="id" ref="panel" class="task-popover" role="group" :aria-label="label" tabindex="-1">
      <slot />
    </div>
  </Teleport>
</template>
