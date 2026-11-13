import { useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Check, CircleNotch, ImageSquare, Trash, UploadSimple } from '@phosphor-icons/react'
import { bookPostersApi, collectionPostersApi, readListPostersApi, seriesPostersApi, smartListPostersApi } from '@/lib/api/posters'
import type { ThumbnailType } from '@/lib/api/types'
import { useBust, useThumbnailBust } from '@/lib/store/thumbnails'
import { formatBytes } from '@/lib/utils/format'
import { cn } from '@/lib/utils/cn'
import { Button } from '@/components/ui/Button'
import { Dialog } from '@/components/ui/Dialog'
import { EmptyState } from '@/components/ui/EmptyState'
import { IconButton } from '@/components/ui/IconButton'
import { GridSkeleton } from '@/components/ui/Skeleton'
import { FormErrorBanner } from './fields'

export type PosterKind = 'series' | 'book' | 'collection' | 'readlist' | 'smartlist'

interface PosterItem {
  id: string
  type: ThumbnailType
  selected: boolean
  mediaType: string
  fileSize: number
  width: number
  height: number
}

interface PosterApi {
  list: (id: string) => Promise<PosterItem[]>
  upload: (id: string, file: Blob, selected?: boolean) => Promise<unknown>
  markSelected: (id: string, thumbId: string) => Promise<void>
  delete: (id: string, thumbId: string) => Promise<void>
  thumbnailUrl: (id: string, thumbId: string) => string
}

const APIS: Record<PosterKind, PosterApi> = {
  series: seriesPostersApi,
  book: bookPostersApi,
  collection: collectionPostersApi,
  readlist: readListPostersApi,
  smartlist: smartListPostersApi,
}

const INVALIDATE: Record<PosterKind, string[][]> = {
  series: [['series'], ['dashboard']],
  book: [['books'], ['series'], ['dashboard']],
  collection: [['collections'], ['dashboard']],
  readlist: [['readlists'], ['dashboard']],
  smartlist: [['smart-lists']],
}

const TYPE_KEYS: Record<ThumbnailType, string> = {
  GENERATED: 'generated',
  SIDECAR: 'sidecar',
  USER_UPLOADED: 'uploaded',
}

export function PosterManager({
  open,
  onClose,
  kind,
  entityId,
  title,
  onChange,
}: {
  open: boolean
  onClose: () => void
  kind: PosterKind
  entityId: string
  title: string
  onChange?: () => void
}) {
  const { t } = useTranslation('metadata')
  const queryClient = useQueryClient()
  const typeLabel = (type: ThumbnailType) => t(`poster.type.${TYPE_KEYS[type]}`)
  const api = APIS[kind]
  const bust = useBust(entityId)
  const bump = useThumbnailBust((s) => s.bump)
  const fileRef = useRef<HTMLInputElement>(null)
  const [armedDelete, setArmedDelete] = useState<string | null>(null)
  const [errors, setErrors] = useState<string[]>([])

  const postersQuery = useQuery({
    queryKey: ['posters', kind, entityId],
    queryFn: () => api.list(entityId),
    enabled: open,
  })
  const posters = postersQuery.data ?? []
  const busy = postersQuery.isFetching

  const afterChange = () => {
    setErrors([])
    setArmedDelete(null)
    queryClient.invalidateQueries({ queryKey: ['posters', kind, entityId] })
    for (const key of INVALIDATE[kind]) queryClient.invalidateQueries({ queryKey: key })
    bump(entityId)
    onChange?.()
  }
  const onError = (err: unknown) => setErrors([err instanceof Error ? err.message : t('poster.operationFailed')])

  const selectMutation = useMutation({ mutationFn: (thumbId: string) => api.markSelected(entityId, thumbId), onSuccess: afterChange, onError })
  const deleteMutation = useMutation({ mutationFn: (thumbId: string) => api.delete(entityId, thumbId), onSuccess: afterChange, onError })
  const uploadMutation = useMutation({
    // auto-select only when nothing is selected yet, so an extra upload never steals the active poster
    mutationFn: (file: Blob) => api.upload(entityId, file, !posters.some((p) => p.selected)),
    onSuccess: afterChange,
    onError,
  })

  const pickFile = () => fileRef.current?.click()

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!o) onClose()
      }}
      title={t('detail:menu.managePosters')}
      size="lg"
    >
      <div className="flex flex-col gap-4 px-5 py-4">
        <FormErrorBanner messages={errors} />
        <div className="flex items-center justify-between gap-4">
          <p className="min-w-0 truncate text-sm text-ink-3">
            {title} · {t(`poster.kind.${kind}`)}
          </p>
          <Button size="sm" loading={uploadMutation.isPending} onClick={pickFile}>
            <UploadSimple className="size-4" />
            {t('poster.upload')}
          </Button>
          <input
            ref={fileRef}
            type="file"
            accept="image/*"
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0]
              if (file) uploadMutation.mutate(file)
              e.target.value = ''
            }}
          />
        </div>

        {postersQuery.isPending ? (
          <GridSkeleton count={4} />
        ) : postersQuery.isLoadingError ? (
          <EmptyState
            title={t('poster.loadFailed')}
            body={postersQuery.error.message}
            action={
              <Button variant="secondary" onClick={() => postersQuery.refetch()}>
                {t('common:action.retry')}
              </Button>
            }
          />
        ) : posters.length === 0 ? (
          <EmptyState
            icon={<ImageSquare weight="duotone" />}
            title={t('poster.empty')}
            body={t('poster.emptyHint')}
            action={
              <Button variant="secondary" onClick={pickFile}>
                <UploadSimple className="size-4" />
                {t('poster.upload')}
              </Button>
            }
          />
        ) : (
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
            {posters.map((p) => {
              const armed = armedDelete === p.id
              const deleting = deleteMutation.isPending && deleteMutation.variables === p.id
              return (
                <div
                  key={p.id}
                  onMouseLeave={() => armed && setArmedDelete(null)}
                  className={cn(
                    'group relative overflow-hidden rounded-lg border bg-raised',
                    p.selected ? 'border-accent ring-2 ring-accent/40' : 'border-line',
                  )}
                >
                  <button
                    type="button"
                    disabled={p.selected || selectMutation.isPending}
                    onClick={() => selectMutation.mutate(p.id)}
                    aria-label={p.selected ? t('poster.selected') : t('poster.setSelected')}
                    className="block w-full cursor-pointer disabled:cursor-default"
                  >
                    <img
                      src={`${api.thumbnailUrl(entityId, p.id)}${bust ? `?${bust}` : ''}`}
                      alt={t('poster.alt', { type: typeLabel(p.type) })}
                      loading="lazy"
                      className="w-full object-cover"
                      style={p.width > 0 && p.height > 0 ? { aspectRatio: `${p.width} / ${p.height}` } : undefined}
                    />
                  </button>

                  {p.selected && (
                    <span className="absolute top-2 left-2 inline-flex items-center gap-1 rounded-full bg-accent px-2 py-0.5 text-[11px] font-medium text-accent-ink">
                      <Check className="size-3" weight="bold" />
                      {t('poster.selectedBadge')}
                    </span>
                  )}

                  <div className="absolute top-2 right-2">
                    {armed ? (
                      <button
                        type="button"
                        disabled={deleting}
                        onClick={() => deleteMutation.mutate(p.id)}
                        className="inline-flex h-7 cursor-pointer items-center gap-1 rounded-lg bg-danger px-2 text-[11px] font-medium text-white transition-colors hover:bg-danger/80 disabled:opacity-50"
                      >
                        {deleting ? <CircleNotch className="size-3 animate-spin" /> : <Trash className="size-3" />}
                        {t('common:action.confirm')}
                      </button>
                    ) : (
                      <IconButton
                        label={t('poster.delete')}
                        onClick={() => setArmedDelete(p.id)}
                        className="size-7 bg-black/60 text-white/85 opacity-0 transition-opacity group-hover:opacity-100 hover:bg-black/80 hover:text-white"
                      >
                        <Trash className="size-3.5" />
                      </IconButton>
                    )}
                  </div>

                  <div className="pointer-events-none absolute inset-x-0 bottom-0 flex items-center justify-between gap-2 bg-gradient-to-t from-black/75 to-transparent px-2 pt-6 pb-1.5 text-[11px] text-white/85">
                    <span>{typeLabel(p.type)}</span>
                    <span className="font-mono">
                      {p.width}×{p.height} · {formatBytes(p.fileSize)}
                    </span>
                  </div>
                </div>
              )
            })}
          </div>
        )}
        {busy && !postersQuery.isPending && <p className="text-xs text-ink-3">{t('poster.refreshing')}</p>}
      </div>
    </Dialog>
  )
}
