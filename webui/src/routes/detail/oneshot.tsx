import { useState, type ReactNode } from 'react'
import { Link, Navigate, useNavigate, useParams } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import {
  ArrowCounterClockwise,
  ArrowSquareOut,
  ArrowsClockwise,
  BookmarkSimple,
  BookOpen,
  Checks,
  Circle,
  DotsThreeVertical,
  EyeSlash,
  FileMagnifyingGlass,
  FolderPlus,
  ImageSquare,
  PencilSimple,
  PlugsConnected,
  Sparkle,
  Trash,
} from '@phosphor-icons/react'
import { booksApi } from '@/lib/api/books'
import { librariesApi } from '@/lib/api/libraries'
import { seriesApi } from '@/lib/api/series'
import { komfApi } from '@/lib/api/komf'
import { canDownload, isAdmin, useAuthStore } from '@/lib/store/auth'
import { useDocumentTitle } from '@/lib/hooks/useDocumentTitle'
import { useKomfIntegration } from '@/lib/hooks/useKomfIntegration'
import { useBust, useThumbnailBust } from '@/lib/store/thumbnails'
import { urls } from '@/lib/utils/urls'
import { readRoute } from '@/lib/utils/nav'
import { convertErrorCodes, mediaIssue } from '@/lib/utils/mediaStatus'
import { formatBytes, formatDate, relativeTime, fileNameFromUrl, filePathFromUrl } from '@/lib/utils/format'
import { cn } from '@/lib/utils/cn'
import { Button } from '@/components/ui/Button'
import { IconButton } from '@/components/ui/IconButton'
import { Tooltip } from '@/components/ui/Tooltip'
import { BackButton } from '@/components/ui/BackButton'
import { useGoBack } from '@/lib/hooks/useGoBack'
import { Menu, MenuItem, MenuSeparator } from '@/components/ui/Menu'
import { ClampedText } from '@/components/ui/ClampedText'
import { EmptyState } from '@/components/ui/EmptyState'
import { CoverImage } from '@/components/media/CoverImage'
import { CollectionCard, ReadListCard } from '@/components/media/SeriesCard'
import { HorizontalRow } from '@/components/media/HorizontalRow'
import { useDensityCardWidth } from '@/lib/store/ui'
import { DetailHero } from '@/components/detail/DetailHero'
import { DetailTitle } from '@/components/detail/DetailTitle'
import { DetailSkeleton } from '@/components/detail/DetailSkeleton'
import { DetailError } from '@/components/detail/DetailError'
import { DetailChipFlow } from '@/components/detail/DetailChipFlow'
import { SeriesMetaLine } from '@/components/detail/SeriesMetaLine'
import { creatorChipItems, genreChipItems, sharingLabelChipItems, tagChipItems } from '@/components/detail/metadataChips'
import { Summary } from '@/components/detail/Summary'
import { DownloadLink } from '@/components/detail/DownloadLink'
import { AddToReadListDialog } from '@/components/detail/AddToReadListDialog'
import { AddToCollectionDialog } from '@/components/browse/AddToCollectionDialog'
import { ConfirmDeleteDialog } from '@/components/detail/ConfirmDeleteDialog'
import { RemoveFromCollectionButton, RemoveFromReadListButton } from '@/components/detail/MembershipRemoveButton'
import { EditSeriesDialog } from '@/components/metadata/EditSeriesDialog'
import { EditBooksDialog } from '@/components/metadata/EditBooksDialog'
import { PosterManager } from '@/components/metadata/PosterManager'
import { KomfIdentifyDialog } from '@/components/metadata/KomfIdentifyDialog'
import { KomfResetDialog } from '@/components/metadata/KomfResetDialog'
import { showToast } from '@/lib/store/toast'
import { trackKomfJob } from '@/lib/store/komfJobs'

function Field({ term, mono, danger, children }: { term: string; mono?: boolean; danger?: boolean; children: ReactNode }) {
  return (
    <div>
      <dt className="text-xs tracking-wide text-ink-3 uppercase">{term}</dt>
      <dd className={cn('mt-1 text-sm', danger ? 'font-medium text-danger' : 'text-ink', mono && 'font-mono')}>{children}</dd>
    </div>
  )
}

export function OneshotDetailPage() {
  const { t, i18n } = useTranslation('detail')
  const { seriesId = '' } = useParams()
  const navigate = useNavigate()
  const goBack = useGoBack()
  const queryClient = useQueryClient()
  const user = useAuthStore((s) => s.user)
  const rowCardWidth = useDensityCardWidth()
  const [addToListOpen, setAddToListOpen] = useState(false)
  const [addToCollectionOpen, setAddToCollectionOpen] = useState(false)
  const [editOpen, setEditOpen] = useState(false)
  const [editBookOpen, setEditBookOpen] = useState(false)
  const [postersOpen, setPostersOpen] = useState(false)
  const [identifyOpen, setIdentifyOpen] = useState(false)
  const [komfResetOpen, setKomfResetOpen] = useState(false)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const komfReady = useKomfIntegration()

  const seriesQuery = useQuery({ queryKey: ['series', seriesId], queryFn: () => seriesApi.get(seriesId) })
  const series = seriesQuery.data
  const title = series ? series.metadata.title || series.name : ''

  // a oneshot series holds exactly one book; the page is built around it
  const bookQuery = useQuery({
    queryKey: ['series', seriesId, 'oneshot-book'],
    queryFn: async () => (await seriesApi.books(seriesId, { size: 1 })).content[0] ?? null,
    enabled: !!series,
  })
  const book = bookQuery.data ?? null
  const bust = useBust(seriesId)
  const bump = useThumbnailBust((s) => s.bump)

  const librariesQuery = useQuery({ queryKey: ['libraries'], queryFn: librariesApi.list })
  const collectionsQuery = useQuery({
    queryKey: ['collections', 'series', seriesId],
    queryFn: () => seriesApi.collections(seriesId),
    enabled: !!series,
  })
  const readlistsQuery = useQuery({
    queryKey: ['readlists', 'book', book?.id],
    queryFn: () => booksApi.readlists(book!.id),
    enabled: !!book,
  })

  const markMutation = useMutation({
    mutationFn: (read: boolean) => (read ? booksApi.markRead(book!.id) : booksApi.markUnread(book!.id)),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['books'] })
      queryClient.invalidateQueries({ queryKey: ['series'] })
      queryClient.invalidateQueries({ queryKey: ['dashboard'] })
      queryClient.invalidateQueries({ queryKey: ['readlists'] })
    },
  })

  const analyzeMutation = useMutation({
    mutationFn: () => booksApi.analyze(book!.id),
    onSuccess: () => showToast(t('toast.analysisQueued')),
    onError: (e) => showToast(e instanceof Error ? e.message : t('toast.queueAnalysisFailed')),
  })
  const refreshMutation = useMutation({
    mutationFn: async () => {
      await booksApi.refreshMetadata(book!.id)
      await seriesApi.refreshMetadata(seriesId)
    },
    onSuccess: () => showToast(t('toast.refreshQueued')),
    onError: (e) => showToast(e instanceof Error ? e.message : t('toast.queueRefreshFailed')),
  })
  // komf PATCHes the metadata asynchronously once the job runs; the SSE SeriesChanged
  // invalidation picks the result up, so no query invalidation here
  const komfMatchMutation = useMutation({
    mutationFn: (libraryId: string) => komfApi.matchSeries(libraryId, seriesId),
    onSuccess: ({ id }) => trackKomfJob(id, title),
    onError: (e) => showToast(e instanceof Error ? e.message : t('toast.queueMatchFailed')),
  })
  const komfResetMutation = useMutation({
    mutationFn: (libraryId: string) => komfApi.resetSeries(libraryId, seriesId),
    onSuccess: () => {
      setKomfResetOpen(false)
      queryClient.invalidateQueries({ queryKey: ['series'] })
      queryClient.invalidateQueries({ queryKey: ['books'] })
      showToast(t('toast.metadataReset'))
    },
    onError: (e) => showToast(e instanceof Error ? e.message : t('toast.resetMetadataFailed')),
  })
  const deleteMutation = useMutation({
    mutationFn: () => booksApi.deleteFile(book!.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['books'] })
      queryClient.invalidateQueries({ queryKey: ['series'] })
      queryClient.invalidateQueries({ queryKey: ['dashboard'] })
      goBack('/series')
    },
    onError: (e) => showToast(e instanceof Error ? e.message : t('toast.deleteFileFailed')),
  })

  useDocumentTitle(title || undefined)

  if (seriesQuery.isPending) return <DetailSkeleton />
  if (seriesQuery.isLoadingError)
    return <DetailError error={seriesQuery.error} notFoundTitle={t('notFound.series')} onRetry={() => seriesQuery.refetch()} />
  if (!series) return null
  if (!series.oneshot) return <Navigate to={`/series/${series.id}`} replace />
  if (bookQuery.isPending) return <DetailSkeleton />
  if (bookQuery.isLoadingError)
    return <DetailError error={bookQuery.error} notFoundTitle={t('notFound.book')} onRetry={() => bookQuery.refetch()} />
  if (!book)
    return (
      <div>
        <BackButton to="/series" className="mb-2 -ml-2" />
        <EmptyState
          className="py-32"
          icon={<BookOpen weight="duotone" />}
          title={t('empty.noBook')}
          body={t('empty.oneshotNoBook')}
        />
      </div>
    )

  const md = series.metadata
  const bookMd = book.metadata
  const library = librariesQuery.data?.find((l) => l.id === series.libraryId)
  const progress = book.readProgress
  const completed = !!progress?.completed
  // an unavailable library makes the book act deleted (komga parity)
  const unavailable = book.deleted || (library?.unavailable ?? false)
  const route = readRoute({ id: book.id, media: book.media, deleted: unavailable })
  const issue = mediaIssue({ media: book.media, deleted: unavailable })
  // the issue line already carries the comment for ERROR/UNSUPPORTED; the field covers the rest
  const mediaComment = book.media.comment && !issue?.detail ? convertErrorCodes(book.media.comment) : ''
  const readlists = readlistsQuery.data ?? []
  const collections = collectionsQuery.data ?? []
  const cover = urls.seriesThumbnail(series.id, bust || undefined)
  const authors = bookMd.authors.length > 0 ? bookMd.authors : series.booksMetadata.authors
  const summary = md.summary || bookMd.summary
  const progressPct = completed ? 100 : book.media.pagesCount > 0 && progress ? (progress.page / book.media.pagesCount) * 100 : 0

  const creatorItems = creatorChipItems(md.publisher, authors, '/series')
  const genreItems = genreChipItems(md.genres)
  // book tags merge into the series flow: a oneshot is one book, so there is nothing to distinguish
  const tagItems = tagChipItems([...new Set([...md.tags, ...bookMd.tags])], '/series', i18n.language)
  const sharingItems = sharingLabelChipItems(md.sharingLabels)
  const links = [...md.links, ...bookMd.links.filter((bl) => !md.links.some((sl) => sl.url === bl.url))]
  const hasMetadataFlows = genreItems.length + tagItems.length + sharingItems.length > 0

  return (
    <div>
      <DetailHero backdrop={cover} leading={<BackButton to="/series" className="mb-2 -ml-2" />}>
        <CoverImage src={cover} alt={title} eager className="w-36 shrink-0 shadow-card md:w-44" />
        <div className="min-w-0 flex-1">
          {library && (
            <p className="text-xs tracking-wide text-ink-3 uppercase">
              <Link to={`/libraries/${library.id}/series`} className="transition-colors hover:text-accent-strong">
                {library.name}
              </Link>
            </p>
          )}
          <DetailTitle title={title} />
          <DetailChipFlow items={creatorItems} className="mt-2.5" />
          <div className="mt-4 flex flex-wrap items-center gap-2">
            <Button variant="primary" disabled={!route} onClick={() => route && navigate(route)}>
              <BookOpen className="size-4" />
              {progress && !progress.completed ? t('continueWithPage', { page: progress.page }) : t('read')}
            </Button>
            <Tooltip content={t('peekTooltip')}>
              <Button variant="secondary" disabled={!route} onClick={() => route && navigate(`${route}?incognito=true`)}>
                <EyeSlash className="size-4" />
                {t('peek')}
              </Button>
            </Tooltip>
            {!completed && (
              <Button variant="secondary" loading={markMutation.isPending} onClick={() => markMutation.mutate(true)}>
                <Checks className="size-4" />
                {t('markAsRead')}
              </Button>
            )}
            {progress && (
              <Button variant="secondary" loading={markMutation.isPending} onClick={() => markMutation.mutate(false)}>
                <Circle className="size-4" />
                {t('markAsUnread')}
              </Button>
            )}
            {canDownload(user) && <DownloadLink href={urls.bookFile(book.id)} disabled={unavailable} />}
            {isAdmin(user) && (
              <Menu
                trigger={
                  <IconButton label={t('moreActions')}>
                    <DotsThreeVertical className="size-5" />
                  </IconButton>
                }
              >
                <MenuItem onSelect={() => setEditOpen(true)}>
                  <PencilSimple className="size-4" /> {t('menu.editMetadata')}
                </MenuItem>
                <MenuItem onSelect={() => setEditBookOpen(true)}>
                  <PencilSimple className="size-4" /> {t('menu.editBookMetadata')}
                </MenuItem>
                <MenuItem onSelect={() => setPostersOpen(true)}>
                  <ImageSquare className="size-4" /> {t('menu.managePosters')}
                </MenuItem>
                <MenuSeparator />
                <MenuItem onSelect={() => analyzeMutation.mutate()} disabled={analyzeMutation.isPending}>
                  <FileMagnifyingGlass className="size-4" /> {t('menu.analyze')}
                </MenuItem>
                <MenuItem onSelect={() => refreshMutation.mutate()} disabled={refreshMutation.isPending}>
                  <ArrowsClockwise className="size-4" /> {t('menu.refreshMetadata')}
                </MenuItem>
                {komfReady && (
                  <>
                    <MenuSeparator />
                    <MenuItem onSelect={() => setIdentifyOpen(true)}>
                      <Sparkle className="size-4" /> {t('menu.identifyKomf')}
                    </MenuItem>
                    <MenuItem
                      onSelect={() => komfMatchMutation.mutate(series.libraryId)}
                      disabled={komfMatchMutation.isPending}
                    >
                      <PlugsConnected className="size-4" /> {t('menu.matchKomf')}
                    </MenuItem>
                    <MenuItem onSelect={() => setKomfResetOpen(true)}>
                      <ArrowCounterClockwise className="size-4" /> {t('menu.resetKomf')}
                    </MenuItem>
                  </>
                )}
                <MenuSeparator />
                <MenuItem onSelect={() => setAddToListOpen(true)}>
                  <BookmarkSimple className="size-4" /> {t('addToReadList')}
                </MenuItem>
                <MenuItem onSelect={() => setAddToCollectionOpen(true)}>
                  <FolderPlus className="size-4" /> {t('addToCollection')}
                </MenuItem>
                <MenuSeparator />
                <MenuItem danger onSelect={() => setDeleteOpen(true)}>
                  <Trash className="size-4" /> {t('menu.deleteFile')}
                </MenuItem>
              </Menu>
            )}
          </div>
          {issue && (
            <p
              className={cn(
                'mt-2 text-xs',
                issue.severity === 'danger'
                  ? 'text-danger'
                  : issue.severity === 'accent'
                    ? 'text-accent-strong'
                    : 'text-ink-3',
              )}
            >
              {issue.title}
              {issue.detail && `: ${issue.detail}`}
            </p>
          )}
        </div>
      </DetailHero>

      <SeriesMetaLine md={md} className="mt-4" />

      {md.alternateTitles.length > 0 && (
        <div className="mt-3 space-y-0.5">
          {md.alternateTitles.map((t, i) => (
            <p key={`${t.label}-${i}`} className="text-xs text-ink-3">
              {t.title} ({t.label})
            </p>
          ))}
        </div>
      )}

      {summary && <Summary text={summary} className="mt-4" />}

      {hasMetadataFlows && (
        <div className="mt-4 space-y-2">
          <DetailChipFlow items={genreItems} />
          <DetailChipFlow items={tagItems} />
          <DetailChipFlow items={sharingItems} />
        </div>
      )}

      {links.length > 0 && (
        <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1">
          {links.map((l) => (
            <a
              key={l.url}
              href={l.url}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 text-xs text-ink-3 transition-colors hover:text-accent-strong"
            >
              {l.label}
              <ArrowSquareOut className="size-3" />
            </a>
          ))}
        </div>
      )}

      <section className="mt-8 rounded-xl border border-line bg-surface p-5">
        <dl className="grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2">
          <Field term={t('field.pages')} mono>
            {book.media.pagesCount}
          </Field>
          <Field term={t('field.size')} mono>
            {formatBytes(book.sizeBytes)}
          </Field>
          <Field term={t('field.format')}>{book.media.mediaType}</Field>
          <Field term={t('field.profile')}>{book.media.mediaProfile}</Field>
          {mediaComment && (
            <Field term={t('field.comment')} danger>
              <ClampedText text={mediaComment} lines={2} />
            </Field>
          )}
          {bookMd.releaseDate && <Field term={t('field.releaseDate')}>{formatDate(bookMd.releaseDate)}</Field>}
          {bookMd.isbn && (
            <Field term="ISBN" mono>
              {bookMd.isbn}
            </Field>
          )}
          <Field term={t('field.file')} mono>
            <Tooltip content={filePathFromUrl(book.url)}>
              <span className="inline-block max-w-full truncate">{fileNameFromUrl(book.url)}</span>
            </Tooltip>
          </Field>
          <Field term={t('field.added')}>{formatDate(book.created)}</Field>
        </dl>

        {progress && (
          <div className="mt-5 border-t border-line pt-4">
            <p className="text-sm text-ink-2">
              {completed ? t('progress.completed') : t('progress.pageOf', { page: progress.page, total: book.media.pagesCount })}
              {progress.readDate && <span className="text-ink-3"> · {relativeTime(progress.readDate)}</span>}
            </p>
            <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-overlay">
              <div className="h-full rounded-full bg-accent transition-[width] duration-300" style={{ width: `${progressPct}%` }} />
            </div>
          </div>
        )}
      </section>

      {collections.length > 0 && (
        <HorizontalRow title={t('inCollections')} className="mt-10">
          {collections.map((c) => (
            <div key={c.id} className="shrink-0" style={{ width: rowCardWidth }}>
              <CollectionCard
                id={c.id}
                name={c.name}
                count={c.seriesIds.length}
                actions={isAdmin(user) ? <RemoveFromCollectionButton collection={c} seriesId={series.id} /> : undefined}
              />
            </div>
          ))}
        </HorizontalRow>
      )}

      {readlists.length > 0 && (
        <HorizontalRow title={t('inReadLists')} className="mt-10">
          {readlists.map((l) => (
            <div key={l.id} className="shrink-0" style={{ width: rowCardWidth }}>
              <ReadListCard
                id={l.id}
                name={l.name}
                count={l.bookIds.length}
                actions={isAdmin(user) ? <RemoveFromReadListButton readlist={l} bookId={book.id} /> : undefined}
              />
            </div>
          ))}
        </HorizontalRow>
      )}

      <AddToReadListDialog bookId={book.id} open={addToListOpen} onOpenChange={setAddToListOpen} />
      <AddToCollectionDialog
        open={addToCollectionOpen}
        onOpenChange={setAddToCollectionOpen}
        seriesIds={[series.id]}
        onDone={(_ok, message) => showToast(message)}
      />
      <EditSeriesDialog open={editOpen} onClose={() => setEditOpen(false)} seriesIds={[series.id]} />
      <EditBooksDialog open={editBookOpen} onClose={() => setEditBookOpen(false)} bookIds={[book.id]} />
      {komfReady && (
        <>
          <KomfIdentifyDialog
            open={identifyOpen}
            onOpenChange={setIdentifyOpen}
            series={series}
            onIdentified={() => showToast(t('toast.identifyQueued'))}
          />
          <KomfResetDialog
            open={komfResetOpen}
            onOpenChange={setKomfResetOpen}
            name={title}
            loading={komfResetMutation.isPending}
            onConfirm={() => komfResetMutation.mutate(series.libraryId)}
          />
        </>
      )}
      <PosterManager
        open={postersOpen}
        onClose={() => setPostersOpen(false)}
        kind="book"
        entityId={book.id}
        title={title}
        onChange={() => bump(series.id)}
      />
      <ConfirmDeleteDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title={t('deleteDialog.bookFileTitle')}
        name={t('deleteDialog.bookFileName', { name: title })}
        loading={deleteMutation.isPending}
        onConfirm={() => deleteMutation.mutate()}
      />
    </div>
  )
}
