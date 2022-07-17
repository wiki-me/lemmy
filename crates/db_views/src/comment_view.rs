use crate::structs::CommentView;
use diesel::{
  dsl::*,
  result::{Error, Error::QueryBuilderError},
  *,
};
use diesel_ltree::{Ltree, LtreeExtensions};
use lemmy_db_schema::{
  aggregates::structs::CommentAggregates,
  newtypes::{CommentId, CommunityId, DbUrl, PersonId, PostId},
  schema::{
    comment,
    comment_aggregates,
    comment_like,
    comment_saved,
    community,
    community_block,
    community_follower,
    community_person_ban,
    person,
    person_block,
    post,
  },
  source::{
    comment::{Comment, CommentSaved},
    community::{Community, CommunityFollower, CommunityPersonBan, CommunitySafe},
    person::{Person, PersonSafe},
    person_block::PersonBlock,
    post::Post,
  },
  traits::{MaybeOptional, ToSafe, ViewToVec},
  utils::{functions::hot_rank, fuzzy_search, limit_and_offset_unlimited},
  ListingType,
  SortType,
};

type CommentViewTuple = (
  Comment,
  PersonSafe,
  Post,
  CommunitySafe,
  CommentAggregates,
  Option<CommunityPersonBan>,
  Option<CommunityFollower>,
  Option<CommentSaved>,
  Option<PersonBlock>,
  Option<i16>,
);

impl CommentView {
  pub fn read(
    conn: &PgConnection,
    comment_id: CommentId,
    my_person_id: Option<PersonId>,
  ) -> Result<Self, Error> {
    // The left join below will return None in this case
    let person_id_join = my_person_id.unwrap_or(PersonId(-1));

    let (
      comment,
      creator,
      post,
      community,
      counts,
      creator_banned_from_community,
      follower,
      saved,
      creator_blocked,
      comment_like,
    ) = comment::table
      .find(comment_id)
      .inner_join(person::table)
      .inner_join(post::table)
      .inner_join(community::table.on(post::community_id.eq(community::id)))
      .inner_join(comment_aggregates::table)
      .left_join(
        community_person_ban::table.on(
          community::id
            .eq(community_person_ban::community_id)
            .and(community_person_ban::person_id.eq(comment::creator_id))
            .and(
              community_person_ban::expires
                .is_null()
                .or(community_person_ban::expires.gt(now)),
            ),
        ),
      )
      .left_join(
        community_follower::table.on(
          post::community_id
            .eq(community_follower::community_id)
            .and(community_follower::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        comment_saved::table.on(
          comment::id
            .eq(comment_saved::comment_id)
            .and(comment_saved::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        person_block::table.on(
          comment::creator_id
            .eq(person_block::target_id)
            .and(person_block::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        comment_like::table.on(
          comment::id
            .eq(comment_like::comment_id)
            .and(comment_like::person_id.eq(person_id_join)),
        ),
      )
      .select((
        comment::all_columns,
        Person::safe_columns_tuple(),
        post::all_columns,
        Community::safe_columns_tuple(),
        comment_aggregates::all_columns,
        community_person_ban::all_columns.nullable(),
        community_follower::all_columns.nullable(),
        comment_saved::all_columns.nullable(),
        person_block::all_columns.nullable(),
        comment_like::score.nullable(),
      ))
      .first::<CommentViewTuple>(conn)?;

    // If a person is given, then my_vote, if None, should be 0, not null
    // Necessary to differentiate between other person's votes
    let my_vote = if my_person_id.is_some() && comment_like.is_none() {
      Some(0)
    } else {
      comment_like
    };

    Ok(CommentView {
      comment,
      post,
      creator,
      community,
      counts,
      creator_banned_from_community: creator_banned_from_community.is_some(),
      subscribed: CommunityFollower::to_subscribed_type(&follower),
      saved: saved.is_some(),
      creator_blocked: creator_blocked.is_some(),
      my_vote,
    })
  }
}

pub struct CommentQueryBuilder<'a> {
  conn: &'a PgConnection,
  listing_type: Option<ListingType>,
  sort: Option<SortType>,
  community_id: Option<CommunityId>,
  community_actor_id: Option<DbUrl>,
  post_id: Option<PostId>,
  parent_path: Option<Ltree>,
  creator_id: Option<PersonId>,
  my_person_id: Option<PersonId>,
  search_term: Option<String>,
  saved_only: Option<bool>,
  show_bot_accounts: Option<bool>,
  page: Option<i64>,
  limit: Option<i64>,
}

impl<'a> CommentQueryBuilder<'a> {
  pub fn create(conn: &'a PgConnection) -> Self {
    CommentQueryBuilder {
      conn,
      listing_type: None,
      sort: None,
      community_id: None,
      community_actor_id: None,
      post_id: None,
      parent_path: None,
      creator_id: None,
      my_person_id: None,
      search_term: None,
      saved_only: None,
      show_bot_accounts: None,
      page: None,
      limit: None,
    }
  }

  pub fn listing_type<T: MaybeOptional<ListingType>>(mut self, listing_type: T) -> Self {
    self.listing_type = listing_type.get_optional();
    self
  }

  pub fn sort<T: MaybeOptional<SortType>>(mut self, sort: T) -> Self {
    self.sort = sort.get_optional();
    self
  }

  pub fn post_id<T: MaybeOptional<PostId>>(mut self, post_id: T) -> Self {
    self.post_id = post_id.get_optional();
    self
  }

  pub fn creator_id<T: MaybeOptional<PersonId>>(mut self, creator_id: T) -> Self {
    self.creator_id = creator_id.get_optional();
    self
  }

  pub fn community_id<T: MaybeOptional<CommunityId>>(mut self, community_id: T) -> Self {
    self.community_id = community_id.get_optional();
    self
  }

  pub fn my_person_id<T: MaybeOptional<PersonId>>(mut self, my_person_id: T) -> Self {
    self.my_person_id = my_person_id.get_optional();
    self
  }

  pub fn community_actor_id<T: MaybeOptional<DbUrl>>(mut self, community_actor_id: T) -> Self {
    self.community_actor_id = community_actor_id.get_optional();
    self
  }

  pub fn search_term<T: MaybeOptional<String>>(mut self, search_term: T) -> Self {
    self.search_term = search_term.get_optional();
    self
  }

  pub fn saved_only<T: MaybeOptional<bool>>(mut self, saved_only: T) -> Self {
    self.saved_only = saved_only.get_optional();
    self
  }

  pub fn show_bot_accounts<T: MaybeOptional<bool>>(mut self, show_bot_accounts: T) -> Self {
    self.show_bot_accounts = show_bot_accounts.get_optional();
    self
  }

  pub fn parent_path<T: MaybeOptional<Ltree>>(mut self, parent_path: T) -> Self {
    self.parent_path = parent_path.get_optional();
    self
  }

  pub fn page<T: MaybeOptional<i64>>(mut self, page: T) -> Self {
    self.page = page.get_optional();
    self
  }

  pub fn limit<T: MaybeOptional<i64>>(mut self, limit: T) -> Self {
    self.limit = limit.get_optional();
    self
  }

  pub fn list(self) -> Result<Vec<CommentView>, Error> {
    use diesel::dsl::*;

    // The left join below will return None in this case
    let person_id_join = self.my_person_id.unwrap_or(PersonId(-1));

    let mut query = comment::table
      .inner_join(person::table)
      .inner_join(post::table)
      .inner_join(community::table.on(post::community_id.eq(community::id)))
      .inner_join(comment_aggregates::table)
      .left_join(
        community_person_ban::table.on(
          community::id
            .eq(community_person_ban::community_id)
            .and(community_person_ban::person_id.eq(comment::creator_id))
            .and(
              community_person_ban::expires
                .is_null()
                .or(community_person_ban::expires.gt(now)),
            ),
        ),
      )
      .left_join(
        community_follower::table.on(
          post::community_id
            .eq(community_follower::community_id)
            .and(community_follower::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        comment_saved::table.on(
          comment::id
            .eq(comment_saved::comment_id)
            .and(comment_saved::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        person_block::table.on(
          comment::creator_id
            .eq(person_block::target_id)
            .and(person_block::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        community_block::table.on(
          community::id
            .eq(community_block::community_id)
            .and(community_block::person_id.eq(person_id_join)),
        ),
      )
      .left_join(
        comment_like::table.on(
          comment::id
            .eq(comment_like::comment_id)
            .and(comment_like::person_id.eq(person_id_join)),
        ),
      )
      .select((
        comment::all_columns,
        Person::safe_columns_tuple(),
        post::all_columns,
        Community::safe_columns_tuple(),
        comment_aggregates::all_columns,
        community_person_ban::all_columns.nullable(),
        community_follower::all_columns.nullable(),
        comment_saved::all_columns.nullable(),
        person_block::all_columns.nullable(),
        comment_like::score.nullable(),
      ))
      .into_boxed();

    if let Some(creator_id) = self.creator_id {
      query = query.filter(comment::creator_id.eq(creator_id));
    };

    if let Some(post_id) = self.post_id {
      query = query.filter(comment::post_id.eq(post_id));
    };

    if let Some(parent_path) = self.parent_path {
      query = query.filter(comment::path.contained_by(parent_path));
    };

    if let Some(search_term) = self.search_term {
      query = query.filter(comment::content.ilike(fuzzy_search(&search_term)));
    };

    if let Some(listing_type) = self.listing_type {
      match listing_type {
        ListingType::Subscribed => {
          query = query.filter(community_follower::person_id.is_not_null())
        } // TODO could be this: and(community_follower::person_id.eq(person_id_join)),
        ListingType::Local => {
          query = query.filter(community::local.eq(true)).filter(
            community::hidden
              .eq(false)
              .or(community_follower::person_id.eq(person_id_join)),
          )
        }
        ListingType::All => {
          query = query.filter(
            community::hidden
              .eq(false)
              .or(community_follower::person_id.eq(person_id_join)),
          )
        }
        ListingType::Community => {
          if self.community_actor_id.is_none() && self.community_id.is_none() {
            return Err(QueryBuilderError("No community actor or id given".into()));
          } else {
            if let Some(community_id) = self.community_id {
              query = query.filter(post::community_id.eq(community_id));
            }

            if let Some(community_actor_id) = self.community_actor_id {
              query = query.filter(community::actor_id.eq(community_actor_id))
            }
          }
        }
      }
    };

    if self.saved_only.unwrap_or(false) {
      query = query.filter(comment_saved::id.is_not_null());
    }

    if !self.show_bot_accounts.unwrap_or(true) {
      query = query.filter(person::bot_account.eq(false));
    };

    query = match self.sort.unwrap_or(SortType::New) {
      SortType::Hot | SortType::Active => query
        .order_by(hot_rank(comment_aggregates::score, comment_aggregates::published).desc())
        .then_order_by(comment_aggregates::published.desc()),
      SortType::New | SortType::MostComments | SortType::NewComments => {
        query.order_by(comment::published.desc())
      }
      SortType::TopAll => query.order_by(comment_aggregates::score.desc()),
      SortType::TopYear => query
        .filter(comment::published.gt(now - 1.years()))
        .order_by(comment_aggregates::score.desc()),
      SortType::TopMonth => query
        .filter(comment::published.gt(now - 1.months()))
        .order_by(comment_aggregates::score.desc()),
      SortType::TopWeek => query
        .filter(comment::published.gt(now - 1.weeks()))
        .order_by(comment_aggregates::score.desc()),
      SortType::TopDay => query
        .filter(comment::published.gt(now - 1.days()))
        .order_by(comment_aggregates::score.desc()),
    };

    // Don't show blocked communities or persons
    if self.my_person_id.is_some() {
      query = query.filter(community_block::person_id.is_null());
      query = query.filter(person_block::person_id.is_null());
    }

    // Don't use the regular error-checking one, many more comments must ofter be fetched.
    let (limit, offset) = limit_and_offset_unlimited(self.page, self.limit);

    // Note: deleted and removed comments are done on the front side
    let res = query
      .limit(limit)
      .offset(offset)
      .load::<CommentViewTuple>(self.conn)?;

    Ok(CommentView::from_tuple_to_vec(res))
  }
}

impl ViewToVec for CommentView {
  type DbTuple = CommentViewTuple;
  fn from_tuple_to_vec(items: Vec<Self::DbTuple>) -> Vec<Self> {
    items
      .iter()
      .map(|a| Self {
        comment: a.0.to_owned(),
        creator: a.1.to_owned(),
        post: a.2.to_owned(),
        community: a.3.to_owned(),
        counts: a.4.to_owned(),
        creator_banned_from_community: a.5.is_some(),
        subscribed: CommunityFollower::to_subscribed_type(&a.6),
        saved: a.7.is_some(),
        creator_blocked: a.8.is_some(),
        my_vote: a.9,
      })
      .collect::<Vec<Self>>()
  }
}

#[cfg(test)]
mod tests {
  use crate::comment_view::*;
  use lemmy_db_schema::{
    aggregates::structs::CommentAggregates,
    source::{comment::*, community::*, person::*, person_block::PersonBlockForm, post::*},
    traits::{Blockable, Crud, Likeable},
    utils::establish_unpooled_connection,
    SubscribedType,
  };
  use serial_test::serial;

  #[test]
  #[serial]
  fn test_crud() {
    let conn = establish_unpooled_connection();

    let new_person = PersonForm {
      name: "timmy".into(),
      public_key: Some("pubkey".to_string()),
      ..PersonForm::default()
    };

    let inserted_person = Person::create(&conn, &new_person).unwrap();

    let new_person_2 = PersonForm {
      name: "sara".into(),
      public_key: Some("pubkey".to_string()),
      ..PersonForm::default()
    };

    let inserted_person_2 = Person::create(&conn, &new_person_2).unwrap();

    let new_community = CommunityForm {
      name: "test community 5".to_string(),
      title: "nada".to_owned(),
      public_key: Some("pubkey".to_string()),
      ..CommunityForm::default()
    };

    let inserted_community = Community::create(&conn, &new_community).unwrap();

    let new_post = PostForm {
      name: "A test post 2".into(),
      creator_id: inserted_person.id,
      community_id: inserted_community.id,
      ..PostForm::default()
    };

    let inserted_post = Post::create(&conn, &new_post).unwrap();

    // Create a comment tree with this hierarchy
    //       0
    //     \     \
    //    1      2
    //    \
    //  3  4
    let comment_form_0 = CommentForm {
      content: "Comment 0".into(),
      creator_id: inserted_person.id,
      post_id: inserted_post.id,
      ..CommentForm::default()
    };

    let mut inserted_comment_0 = Comment::create(&conn, &comment_form_0).unwrap();
    inserted_comment_0 = Comment::update_ltree_path(&conn, inserted_comment_0.id, None).unwrap();

    let comment_form_1 = CommentForm {
      content: "Comment 1, A test blocked comment".into(),
      creator_id: inserted_person_2.id,
      post_id: inserted_post.id,
      ..CommentForm::default()
    };

    let mut inserted_comment_1 = Comment::create(&conn, &comment_form_1).unwrap();
    inserted_comment_1 = Comment::update_ltree_path(
      &conn,
      inserted_comment_1.id,
      Some(inserted_comment_0.to_owned()),
    )
    .unwrap();

    let comment_form_2 = CommentForm {
      content: "Comment 2".into(),
      creator_id: inserted_person.id,
      post_id: inserted_post.id,
      ..CommentForm::default()
    };

    let mut inserted_comment_2 = Comment::create(&conn, &comment_form_2).unwrap();
    inserted_comment_2 = Comment::update_ltree_path(
      &conn,
      inserted_comment_2.id,
      Some(inserted_comment_0.to_owned()),
    )
    .unwrap();

    let comment_form_3 = CommentForm {
      content: "Comment 3".into(),
      creator_id: inserted_person.id,
      post_id: inserted_post.id,
      ..CommentForm::default()
    };

    let mut _inserted_comment_3 = Comment::create(&conn, &comment_form_3).unwrap();
    _inserted_comment_3 = Comment::update_ltree_path(
      &conn,
      _inserted_comment_3.id,
      Some(inserted_comment_1.to_owned()),
    )
    .unwrap();

    let comment_form_4 = CommentForm {
      content: "Comment 4".into(),
      creator_id: inserted_person.id,
      post_id: inserted_post.id,
      ..CommentForm::default()
    };

    let mut _inserted_comment_4 = Comment::create(&conn, &comment_form_4).unwrap();
    _inserted_comment_4 = Comment::update_ltree_path(
      &conn,
      _inserted_comment_4.id,
      Some(inserted_comment_1.to_owned()),
    )
    .unwrap();

    let timmy_blocks_sara_form = PersonBlockForm {
      person_id: inserted_person.id,
      target_id: inserted_person_2.id,
    };

    let inserted_block = PersonBlock::block(&conn, &timmy_blocks_sara_form).unwrap();

    let expected_block = PersonBlock {
      id: inserted_block.id,
      person_id: inserted_person.id,
      target_id: inserted_person_2.id,
      published: inserted_block.published,
    };

    assert_eq!(expected_block, inserted_block);

    let comment_like_form = CommentLikeForm {
      comment_id: inserted_comment_0.id,
      post_id: inserted_post.id,
      person_id: inserted_person.id,
      score: 1,
    };

    let _inserted_comment_like = CommentLike::like(&conn, &comment_like_form).unwrap();

    let agg = CommentAggregates::read(&conn, inserted_comment_0.id).unwrap();

    let top_path = inserted_comment_0.to_owned().path;
    let expected_comment_view_no_person = CommentView {
      creator_banned_from_community: false,
      my_vote: None,
      subscribed: SubscribedType::NotSubscribed,
      saved: false,
      creator_blocked: false,
      comment: Comment {
        id: inserted_comment_0.id,
        content: "Comment 0".into(),
        creator_id: inserted_person.id,
        post_id: inserted_post.id,
        removed: false,
        deleted: false,
        published: inserted_comment_0.published,
        ap_id: inserted_comment_0.ap_id,
        updated: None,
        local: true,
        path: top_path,
      },
      creator: PersonSafe {
        id: inserted_person.id,
        name: "timmy".into(),
        display_name: None,
        published: inserted_person.published,
        avatar: None,
        actor_id: inserted_person.actor_id.to_owned(),
        local: true,
        banned: false,
        deleted: false,
        admin: false,
        bot_account: false,
        bio: None,
        banner: None,
        updated: None,
        inbox_url: inserted_person.inbox_url.to_owned(),
        shared_inbox_url: None,
        matrix_user_id: None,
        ban_expires: None,
      },
      post: Post {
        id: inserted_post.id,
        name: inserted_post.name.to_owned(),
        creator_id: inserted_person.id,
        url: None,
        body: None,
        published: inserted_post.published,
        updated: None,
        community_id: inserted_community.id,
        removed: false,
        deleted: false,
        locked: false,
        stickied: false,
        nsfw: false,
        embed_title: None,
        embed_description: None,
        embed_video_url: None,
        thumbnail_url: None,
        ap_id: inserted_post.ap_id.to_owned(),
        local: true,
      },
      community: CommunitySafe {
        id: inserted_community.id,
        name: "test community 5".to_string(),
        icon: None,
        removed: false,
        deleted: false,
        nsfw: false,
        actor_id: inserted_community.actor_id.to_owned(),
        local: true,
        title: "nada".to_owned(),
        description: None,
        updated: None,
        banner: None,
        hidden: false,
        posting_restricted_to_mods: false,
        published: inserted_community.published,
      },
      counts: CommentAggregates {
        id: agg.id,
        comment_id: inserted_comment_0.id,
        score: 1,
        upvotes: 1,
        downvotes: 0,
        published: agg.published,
        child_count: 4,
      },
    };

    let mut expected_comment_view_with_person = expected_comment_view_no_person.to_owned();
    expected_comment_view_with_person.my_vote = Some(1);

    let mut read_comment_views_no_person = CommentQueryBuilder::create(&conn)
      .post_id(inserted_post.id)
      .list()
      .unwrap();
    read_comment_views_no_person.reverse();

    let mut read_comment_views_with_person = CommentQueryBuilder::create(&conn)
      .post_id(inserted_post.id)
      .my_person_id(inserted_person.id)
      .list()
      .unwrap();
    read_comment_views_with_person.reverse();

    let read_comment_from_blocked_person =
      CommentView::read(&conn, inserted_comment_1.id, Some(inserted_person.id)).unwrap();

    let top_path = inserted_comment_0.path;
    let read_comment_views_top_path = CommentQueryBuilder::create(&conn)
      .post_id(inserted_post.id)
      .parent_path(top_path)
      .list()
      .unwrap();

    let child_path = inserted_comment_1.to_owned().path;
    let read_comment_views_child_path = CommentQueryBuilder::create(&conn)
      .post_id(inserted_post.id)
      .parent_path(child_path)
      .list()
      .unwrap();

    let like_removed =
      CommentLike::remove(&conn, inserted_person.id, inserted_comment_0.id).unwrap();
    let num_deleted = Comment::delete(&conn, inserted_comment_0.id).unwrap();
    Comment::delete(&conn, inserted_comment_1.id).unwrap();
    Post::delete(&conn, inserted_post.id).unwrap();
    Community::delete(&conn, inserted_community.id).unwrap();
    Person::delete(&conn, inserted_person.id).unwrap();
    Person::delete(&conn, inserted_person_2.id).unwrap();

    // Make sure its 1, not showing the blocked comment
    assert_eq!(4, read_comment_views_with_person.len());

    assert_eq!(
      expected_comment_view_no_person,
      read_comment_views_no_person[0]
    );
    assert_eq!(
      expected_comment_view_with_person,
      read_comment_views_with_person[0]
    );

    // Make sure the comment parent-limited fetch is correct
    assert_eq!(5, read_comment_views_top_path.len());
    assert_eq!(3, read_comment_views_child_path.len());

    // Make sure it contains the parent, but not the comment from the other tree
    let child_comments = read_comment_views_child_path
      .into_iter()
      .map(|c| c.comment)
      .collect::<Vec<Comment>>();
    assert!(child_comments.contains(&inserted_comment_1));
    assert!(!child_comments.contains(&inserted_comment_2));

    // Make sure block set the creator blocked
    assert!(read_comment_from_blocked_person.creator_blocked);

    assert_eq!(1, num_deleted);
    assert_eq!(1, like_removed);
  }
}
