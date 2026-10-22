//! 搜索结果的只读展示索引:与结果一起在后台构建,不在重绘时重新分组。

use std::collections::HashMap;
use std::ops::Deref;

use rsou_lib::search::{DocumentHit, SearchResponse};

pub(crate) struct SearchGroup {
    pub id: i64,
    representative: usize,
    /// 下标保持搜索返回的位置顺序,不复制文档元数据或正文片段。
    pub locations: Vec<usize>,
}

pub(crate) struct SearchResults {
    response: SearchResponse,
    pub groups: Vec<SearchGroup>,
    group_indices: HashMap<i64, usize>,
    document_indices: HashMap<i64, usize>,
}

impl SearchResults {
    pub fn new(response: SearchResponse) -> Self {
        let mut groups = Vec::new();
        let mut group_indices = HashMap::new();
        // 与 representatives() 保持相同的顺序,不依赖 HashMap 的迭代顺序。
        for (index, hit) in response.documents.iter().enumerate() {
            if hit.document.id == hit.group_id {
                group_indices.insert(hit.group_id, groups.len());
                groups.push(SearchGroup {
                    id: hit.group_id,
                    representative: index,
                    locations: Vec::new(),
                });
            }
        }
        let mut document_indices = HashMap::with_capacity(response.documents.len());
        for (index, hit) in response.documents.iter().enumerate() {
            document_indices.insert(hit.document.id, index);
            if let Some(&group) = group_indices.get(&hit.group_id) {
                groups[group].locations.push(index);
            }
        }
        Self {
            response,
            groups,
            group_indices,
            document_indices,
        }
    }

    pub fn document(&self, id: i64) -> Option<&DocumentHit> {
        self.document_indices
            .get(&id)
            .map(|&index| &self.response.documents[index])
    }

    pub fn group(&self, id: i64) -> Option<&SearchGroup> {
        self.group_indices
            .get(&id)
            .map(|&index| &self.groups[index])
    }

    pub fn selected(&self, group: &SearchGroup, selected: Option<i64>) -> &DocumentHit {
        selected
            .and_then(|id| self.document(id))
            .filter(|hit| hit.group_id == group.id)
            .unwrap_or(&self.response.documents[group.representative])
    }
}

// 只允许读原结果;若允许原地修改 documents,下标就会失效。
impl Deref for SearchResults {
    type Target = SearchResponse;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use rsou_lib::query::CompiledQuery;
    use rsou_lib::repo::DocumentRow;

    pub(in crate::app) fn doc(id: i64) -> DocumentHit {
        DocumentHit {
            group_id: id,
            document: DocumentRow {
                id,
                path: format!("/doc{id}.txt"),
                file_name: format!("doc{id}.txt"),
                title: String::new(),
                ext: "txt".to_owned(),
                file_type: "text".to_owned(),
                file_size: 1,
                file_mtime_ms: 0,
                parse_status: "parsed".to_owned(),
                parse_error_code: None,
                parse_error_message: None,
                text_length: 1,
                chunk_count: 1,
                indexed_at: None,
                source_root: None,
                updated_at: 0,
            },
            title_highlights: Vec::new(),
            hits: Vec::new(),
            total_hits: 0,
            best_rank: 0.0,
        }
    }

    pub(in crate::app) fn response(ids: &[i64]) -> SearchResponse {
        SearchResponse {
            locations_complete: false,
            documents: ids.iter().map(|&id| doc(id)).collect(),
            total_hits: 0,
            total_documents: ids.len(),
            total_groups: ids.len(),
            elapsed_ms: 0.0,
            compiled: CompiledQuery {
                match_expr: String::new(),
                literals: Vec::new(),
            },
        }
    }

    #[test]
    fn index_preserves_representative_and_location_order_without_copying_documents() {
        let mut response = response(&[20, 4, 3, 19, 5, 6]);
        response.documents[0].group_id = 3;
        response.documents[3].group_id = 4;
        response.documents[5].group_id = 3;
        let original = response.documents.as_ptr();
        let result = SearchResults::new(response);
        assert_eq!(result.documents.as_ptr(), original);
        assert_eq!(
            result.groups.iter().map(|g| g.id).collect::<Vec<_>>(),
            [4, 3, 5]
        );
        for group in &result.groups {
            let expected: Vec<_> = result.locations(group.id).map(|d| d.document.id).collect();
            let actual: Vec<_> = group
                .locations
                .iter()
                .map(|&i| result.documents[i].document.id)
                .collect();
            assert_eq!(actual, expected);
        }
        let group = result.group(3).unwrap();
        assert_eq!(result.selected(group, None).document.id, 3);
        assert_eq!(result.selected(group, Some(20)).document.id, 20);
        assert_eq!(result.selected(group, Some(4)).document.id, 3);
        assert_eq!(result.selected(group, Some(999)).document.id, 3);
        assert!(result.document(999).is_none());
        assert!(result.group(999).is_none());
    }

    #[test]
    fn empty_result_has_no_stale_indices() {
        let result = SearchResults::new(response(&[]));
        assert!(result.groups.is_empty());
        assert!(result.group(1).is_none());
        assert!(result.document(1).is_none());
    }

    #[test]
    fn large_duplicate_groups_match_legacy_iteration() {
        let mut response = response(&(0..10_000).collect::<Vec<_>>());
        for hit in &mut response.documents {
            hit.group_id = hit.document.id % 100;
        }
        let result = SearchResults::new(response);
        assert_eq!(result.groups.len(), 100);
        for group in &result.groups {
            assert_eq!(group.locations.len(), 100);
            for (&index, expected) in group.locations.iter().zip(result.locations(group.id)) {
                assert!(std::ptr::eq(&result.documents[index], expected));
            }
        }
    }
}
