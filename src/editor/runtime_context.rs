//! Runtime context synchronization for blocks, references, images, and focus.

use super::*;

impl Editor {
    /// Chooses a live active target, then pending focus, then the first focusable block.
    pub(super) fn current_edit_target_entity_id_from_state(&self, cx: &App) -> Option<EntityId> {
        self.active_entity_id
            .filter(|entity_id| self.focusable_entity_by_id(*entity_id).is_some())
            .or_else(|| {
                self.pending_focus
                    .filter(|entity_id| self.focusable_entity_by_id(*entity_id).is_some())
            })
            .or_else(|| self.first_focusable_entity_id(cx))
    }

    /// Resolves the state-based edit target without consulting window focus.
    pub(super) fn current_edit_target_from_state(&self, cx: &App) -> Option<Entity<Block>> {
        self.current_edit_target_entity_id_from_state(cx)
            .and_then(|entity_id| self.focusable_entity_by_id(entity_id))
    }

    /// Ends pointer selection in the current target, tree blocks, and table cells.
    /// Returns whether any session changed, optionally notifying the editor.
    fn end_block_pointer_selection_sessions_inner(
        &mut self,
        cx: &mut Context<Self>,
        notify: bool,
    ) -> bool {
        let mut changed = false;

        if let Some(target) = self.current_edit_target_from_state(cx) {
            target.update(cx, |block, _cx| {
                changed |= block.end_pointer_selection_session();
            });
        }

        for visible in self.document.visible_blocks().to_vec() {
            visible.entity.update(cx, |block, _cx| {
                changed |= block.end_pointer_selection_session();
            });
        }

        // Collect only the cell Entity handles, not the whole TableCellBinding
        // (3-field struct of Entity<Block> + TableCellPosition). The collect()
        // exists to drop the &self borrow before the .update() loop; cloning
        // an Entity is an Arc bump, so we pay that once per cell either way —
        // skipping the surrounding struct clone makes the per-frame work
        // proportional to "cell count" not "binding count + position copy".
        let cells: Vec<Entity<Block>> = self
            .table_cells
            .values()
            .map(|binding| binding.cell.clone())
            .collect();
        for cell in cells {
            cell.update(cx, |block, _cx| {
                changed |= block.end_pointer_selection_session();
            });
        }

        if changed && notify {
            cx.notify();
        }
        changed
    }

    /// Ends all block pointer-selection sessions and notifies only if one changed.
    pub(super) fn end_block_pointer_selection_sessions(&mut self, cx: &mut Context<Self>) -> bool {
        self.end_block_pointer_selection_sessions_inner(cx, true)
    }

    /// Creates a new block entity and subscribes this editor to its
    /// [`BlockEvent`](crate::components::BlockEvent) stream.
    pub(super) fn new_block(cx: &mut Context<Self>, record: BlockRecord) -> Entity<Block> {
        let block = cx.new(|cx| Block::with_record(cx, record));
        cx.subscribe(&block, Self::on_block_event).detach();
        block
    }

    /// Creates a subscribed paragraph editor configured for a table cell's position/alignment.
    pub(super) fn new_table_cell_block(
        cx: &mut Context<Self>,
        title: InlineTextTree,
        position: TableCellPosition,
        alignment: TableColumnAlignment,
    ) -> Entity<Block> {
        let block = Self::new_block(cx, BlockRecord::new(BlockKind::Paragraph, title));
        block.update(cx, |block, _cx| {
            block.set_table_cell_mode(position, alignment);
        });
        block
    }

    /// Uses the document's parent directory, falling back to the working directory if available.
    pub(super) fn image_base_dir(&self) -> Option<PathBuf> {
        self.file_path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| std::env::current_dir().ok())
    }

    /// Installs shared references and a base directory on a block, then notifies it.
    pub(super) fn sync_runtime_context_for_block(
        &self,
        block: &Entity<Block>,
        base_dir: Option<&Path>,
        cx: &mut Context<Self>,
    ) {
        // Blocks keep their own image runtime in sync when their content
        // changes, so a block whose document-scoped context already matches
        // needs no work here. Skipping the update also skips the `cx.notify()`
        // that would otherwise dirty the view for every block in the document
        // on every rebuild.
        if block.read(cx).runtime_context_matches(
            base_dir,
            &self.image_reference_definitions,
            &self.link_reference_definitions,
            &self.footnote_registry,
        ) {
            return;
        }
        let next_base_dir = base_dir.map(Path::to_path_buf);
        let image_reference_definitions = self.image_reference_definitions.clone();
        let link_reference_definitions = self.link_reference_definitions.clone();
        let footnote_registry = self.footnote_registry.clone();
        block.update(cx, move |block, cx| {
            block.set_runtime_context(
                next_base_dir.clone(),
                image_reference_definitions.clone(),
                link_reference_definitions.clone(),
                footnote_registry.clone(),
            );
            cx.notify();
        });
    }

    /// Resolves eligible footnote definitions and numbers references in visible order.
    /// First definitions win; unresolved references retain no ordinal.
    pub(super) fn rebuild_footnote_registry(&mut self, cx: &App) {
        let mut definitions = HashMap::new();
        let visible = self.document.visible_blocks().to_vec();
        for visible_block in &visible {
            let block = visible_block.entity.read(cx);
            if block.kind() != BlockKind::FootnoteDefinition {
                continue;
            }

            let allow_definition = self
                .document
                .find_block_location(visible_block.entity.entity_id())
                .is_some_and(|location| {
                    location.parent.is_none()
                        || location
                            .parent
                            .as_ref()
                            .is_some_and(|parent| parent.read(cx).kind().is_quote_container())
                });
            if !allow_definition {
                continue;
            }

            definitions
                .entry(block.record.title.visible_text().to_string())
                .or_insert(visible_block.entity.entity_id());
        }

        let mut bindings = HashMap::<String, FootnoteDefinitionBinding>::new();
        for (id, entity_id) in definitions {
            bindings.insert(
                id,
                FootnoteDefinitionBinding {
                    ordinal: None,
                    definition_entity_id: entity_id,
                    first_reference: None,
                },
            );
        }

        let mut next_ordinal = 1usize;
        let mut occurrence_index = 0usize;
        let mut block_occurrences = HashMap::<uuid::Uuid, Vec<FootnoteResolvedOccurrence>>::new();
        for visible_block in visible {
            let block = visible_block.entity.read(cx);
            let block_id = block.record.id;
            for fragment in &block.record.title.fragments {
                let Some(footnote) = fragment.footnote.as_ref() else {
                    continue;
                };
                let ordinal = if let Some(binding) = bindings.get_mut(&footnote.id) {
                    if binding.ordinal.is_none() {
                        binding.ordinal = Some(next_ordinal);
                        next_ordinal += 1;
                    }
                    if binding.first_reference.is_none() {
                        binding.first_reference = Some(FootnoteReferenceLocation {
                            entity_id: visible_block.entity.entity_id(),
                            occurrence_index,
                        });
                    }
                    binding.ordinal
                } else {
                    None
                };
                block_occurrences
                    .entry(block_id)
                    .or_default()
                    .push(FootnoteResolvedOccurrence {
                        id: footnote.id.clone(),
                        ordinal,
                        occurrence_index,
                    });
                if ordinal.is_none() {
                    occurrence_index += 1;
                    continue;
                }
                occurrence_index += 1;
            }
        }

        let next_registry = FootnoteRegistry {
            bindings,
            block_occurrences,
        };
        // Content-equal registries are interchangeable, so keep the existing
        // allocation. Blocks hold the registry by `Arc`, and pointer identity
        // lets `sync_runtime_context_for_block` skip them without a deep
        // comparison of every block's context.
        if next_registry != *self.footnote_registry {
            self.footnote_registry = Arc::new(next_registry);
        }
    }

    /// Reparses document references and distributes their context to blocks and table cells.
    pub(super) fn rebuild_image_runtimes(&mut self, cx: &mut Context<Self>) {
        let base_dir = self.image_base_dir();
        let markdown = self.document.markdown_text(cx);
        let image_reference_definitions = parse_image_reference_definitions(&markdown);
        if image_reference_definitions != *self.image_reference_definitions {
            self.image_reference_definitions = Arc::new(image_reference_definitions);
        }
        let link_reference_definitions = parse_link_reference_definitions(&markdown);
        if link_reference_definitions != *self.link_reference_definitions {
            self.link_reference_definitions = Arc::new(link_reference_definitions);
        }
        self.rebuild_footnote_registry(cx);
        let visible = self.document.visible_blocks().to_vec();
        for visible_block in visible {
            self.sync_runtime_context_for_block(&visible_block.entity, base_dir.as_deref(), cx);
            if visible_block.entity.read(cx).kind() != BlockKind::Table {
                continue;
            }
            let Some(runtime) = visible_block.entity.read(cx).table_runtime.clone() else {
                continue;
            };
            for cell in runtime.header {
                self.sync_runtime_context_for_block(&cell, base_dir.as_deref(), cx);
            }
            for row in runtime.rows {
                for cell in row {
                    self.sync_runtime_context_for_block(&cell, base_dir.as_deref(), cx);
                }
            }
        }
    }

    /// Finds a tree block or separately registered table-cell editor by entity id.
    pub(super) fn focusable_entity_by_id(&self, entity_id: EntityId) -> Option<Entity<Block>> {
        self.document.block_entity_by_id(entity_id).or_else(|| {
            self.table_cells
                .get(&entity_id)
                .map(|binding| binding.cell.clone())
        })
    }

    /// Chooses the first root, preferring its first header cell when it is a table.
    pub(super) fn first_focusable_entity_id(&self, cx: &App) -> Option<EntityId> {
        let first_root = self.document.first_root()?.clone();
        if first_root.read(cx).kind() == BlockKind::Table {
            return first_root
                .read(cx)
                .table_runtime
                .as_ref()
                .and_then(|runtime| runtime.header.first())
                .map(|cell| cell.entity_id())
                .or_else(|| Some(first_root.entity_id()));
        }
        Some(first_root.entity_id())
    }

    /// Finds actual window focus in the tree first, then in registered table cells.
    pub(super) fn focused_edit_target_entity_id(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<EntityId> {
        self.document
            .focused_block_entity_id(window, cx)
            .or_else(|| {
                self.table_cells
                    .values()
                    .find(|binding| binding.cell.read(cx).focus_handle.is_focused(window))
                    .map(|binding| binding.cell.entity_id())
            })
    }

    /// Resolves the block or table cell that currently holds window focus.
    pub(super) fn focused_edit_target(&self, window: &Window, cx: &App) -> Option<Entity<Block>> {
        self.focused_edit_target_entity_id(window, cx)
            .and_then(|entity_id| self.focusable_entity_by_id(entity_id))
    }

    /// Clones a registered cell's table owner and coordinate binding, if present.
    pub(super) fn table_cell_binding(&self, entity_id: EntityId) -> Option<TableCellBinding> {
        self.table_cells.get(&entity_id).cloned()
    }

    /// Resolves a tree entity only if its current semantic kind is a table.
    pub(super) fn table_block_by_id(&self, entity_id: EntityId, cx: &App) -> Option<Entity<Block>> {
        self.document
            .block_entity_by_id(entity_id)
            .filter(|block| block.read(cx).kind() == BlockKind::Table)
    }
}
