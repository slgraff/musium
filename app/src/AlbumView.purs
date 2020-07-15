-- Mindec -- Music metadata indexer
-- Copyright 2020 Ruud van Asseldonk
--
-- Licensed under the Apache License, Version 2.0 (the "License");
-- you may not use this file except in compliance with the License.
-- A copy of the License has been included in the root of the repository.

module AlbumView
  ( renderAlbum
  ) where

import Control.Monad.Reader.Class (ask)
import Data.Foldable (traverse_)
import Data.String.CodeUnits as CodeUnits
import Effect.Aff (launchAff_)
import Effect.Class (liftEffect)
import Prelude

import Html (Html)
import Html as Html
import Model (Album (..), Track (..))
import Model as Model

renderAlbum :: Album -> Html Unit
renderAlbum (Album album) =
  Html.div $ do
    Html.addClass "album-view"
    Html.div $ do
      Html.addClass "album-info"
      Html.img (Model.thumbUrl album.id) (album.title <> " by " <> album.artist) $ do
        Html.addClass "cover"
      Html.hgroup $ do
        Html.h1 $ Html.text album.title
        Html.h2 $ Html.text album.artist
        Html.h3 $ do
          Html.setTitle album.date
          -- The date is of the form YYYY-MM-DD in ascii, so we can safely take
          -- the first 4 characters to get the year.
          Html.text (CodeUnits.take 4 album.date)

    trackList <- Html.ul $ do
      Html.addClass "track-list"
      ask

    liftEffect $ launchAff_ $ do
      tracks <- Model.getTracks album.id
      let
        lis :: Html Unit
        lis = traverse_ (renderTrack $ Album album) tracks
      liftEffect $ Html.withElement trackList lis

renderTrack :: Album -> Track -> Html Unit
renderTrack album (Track track) =
  Html.li $ do
    Html.addClass "track"

    Html.div $ do
      Html.addClass "track-number"
      Html.text $ show track.trackNumber
    Html.div $ do
      Html.addClass "title"
      Html.text track.title
    Html.div $ do
      Html.addClass "duration"
      Html.text $ Model.formatDurationSeconds track.durationSeconds
    Html.div $ do
      Html.addClass "artist"
      Html.text track.artist
