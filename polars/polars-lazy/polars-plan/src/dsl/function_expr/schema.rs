use super::*;

impl FunctionExpr {
    pub(crate) fn get_field(
        &self,
        _input_schema: &Schema,
        _cntxt: Context,
        fields: &[Field],
    ) -> PolarsResult<Field> {
        use FunctionExpr::*;

        let mapper = FieldsMapper { fields };
        match self {
            #[cfg(feature = "abs")]
            Abs => mapper.with_same_dtype(),
            NullCount => mapper.with_dtype(IDX_DTYPE),
            Pow(_) => mapper.map_to_float_dtype(),
            Coalesce => mapper.map_to_supertype(),
            #[cfg(feature = "row_hash")]
            Hash(..) => mapper.with_dtype(DataType::UInt64),
            #[cfg(feature = "arg_where")]
            ArgWhere => mapper.with_dtype(IDX_DTYPE),
            #[cfg(feature = "search_sorted")]
            SearchSorted(_) => mapper.with_dtype(IDX_DTYPE),
            #[cfg(feature = "strings")]
            StringExpr(s) => s.get_field(mapper),
            BinaryExpr(s) => {
                use BinaryFunction::*;
                match s {
                    Contains { .. } | EndsWith(_) | StartsWith(_) => {
                        mapper.with_dtype(DataType::Boolean)
                    }
                }
            }
            #[cfg(feature = "temporal")]
            TemporalExpr(fun) => {
                use TemporalFunction::*;
                let dtype = match fun {
                    Year | IsoYear => DataType::Int32,
                    Month | Quarter | Week | WeekDay | Day | OrdinalDay | Hour | Minute
                    | Millisecond | Microsecond | Nanosecond | Second => DataType::UInt32,
                    TimeStamp(_) => DataType::Int64,
                    IsLeapYear => DataType::Boolean,
                    Time => DataType::Time,
                    Date => DataType::Date,
                    Datetime => match mapper.with_same_dtype().unwrap().dtype {
                        DataType::Datetime(tu, _) => DataType::Datetime(tu, None),
                        dtype => polars_bail!(ComputeError: "expected Datetime, got {}", dtype),
                    },
                    Truncate(..) => mapper.with_same_dtype().unwrap().dtype,
                    #[cfg(feature = "date_offset")]
                    MonthStart => mapper.with_same_dtype().unwrap().dtype,
                    #[cfg(feature = "date_offset")]
                    MonthEnd => mapper.with_same_dtype().unwrap().dtype,
                    #[cfg(feature = "timezones")]
                    BaseUtcOffset => DataType::Duration(TimeUnit::Milliseconds),
                    #[cfg(feature = "timezones")]
                    DSTOffset => DataType::Duration(TimeUnit::Milliseconds),
                    Round(..) => mapper.with_same_dtype().unwrap().dtype,
                    #[cfg(feature = "timezones")]
                    CastTimezone(tz, _use_earliest) => {
                        return mapper.map_datetime_dtype_timezone(tz.as_ref())
                    }
                    #[cfg(feature = "timezones")]
                    TzLocalize(tz) => return mapper.map_datetime_dtype_timezone(Some(tz)),
                    DateRange {
                        every,
                        closed: _,
                        time_unit,
                        tz,
                    } => {
                        // output dtype may change based on `every`, `tz`, and `time_unit`
                        return mapper.map_to_date_range_dtype(every, time_unit, tz);
                    }
                    TimeRange { .. } => {
                        return Ok(Field::new("time", DataType::List(Box::new(DataType::Time))));
                    }
                    Combine(tu) => match mapper.with_same_dtype().unwrap().dtype {
                        DataType::Datetime(_, tz) => DataType::Datetime(*tu, tz),
                        DataType::Date => DataType::Datetime(*tu, None),
                        dtype => {
                            polars_bail!(ComputeError: "expected Date or Datetime, got {}", dtype)
                        }
                    },
                };
                mapper.with_dtype(dtype)
            }

            #[cfg(feature = "range")]
            Range(fun) => {
                use RangeFunction::*;
                let field = match fun {
                    ARange { .. } => Field::new("arange", DataType::Int64), // This is not always correct
                    IntRange { .. } => Field::new("int", DataType::Int64),
                    IntRanges { .. } => {
                        Field::new("int_range", DataType::List(Box::new(DataType::Int64)))
                    }
                };
                Ok(field)
            }
            #[cfg(feature = "date_offset")]
            DateOffset(_) => mapper.with_same_dtype(),
            #[cfg(feature = "trigonometry")]
            Trigonometry(_) => mapper.map_to_float_dtype(),
            #[cfg(feature = "sign")]
            Sign => mapper.with_dtype(DataType::Int64),
            FillNull { super_type, .. } => mapper.with_dtype(super_type.clone()),
            #[cfg(all(feature = "rolling_window", feature = "moment"))]
            RollingSkew { .. } => mapper.map_to_float_dtype(),
            ShiftAndFill { .. } => mapper.with_same_dtype(),
            DropNans => mapper.with_same_dtype(),
            #[cfg(feature = "round_series")]
            Clip { .. } => mapper.with_same_dtype(),
            ListExpr(l) => {
                use ListFunction::*;
                match l {
                    Concat => mapper.map_to_list_supertype(),
                    #[cfg(feature = "is_in")]
                    Contains => mapper.with_dtype(DataType::Boolean),
                    Slice => mapper.with_same_dtype(),
                    Get => mapper.map_to_list_inner_dtype(),
                    #[cfg(feature = "list_take")]
                    Take(_) => mapper.with_same_dtype(),
                    #[cfg(feature = "list_count")]
                    CountMatch => mapper.with_dtype(IDX_DTYPE),
                    Sum => mapper.nested_sum_type(),
                    #[cfg(feature = "list_sets")]
                    SetOperation(_) => mapper.with_same_dtype(),
                    #[cfg(feature = "list_any_all")]
                    Any => mapper.with_dtype(DataType::Boolean),
                    #[cfg(feature = "list_any_all")]
                    All => mapper.with_dtype(DataType::Boolean),
                }
            }
            #[cfg(feature = "dtype-array")]
            ArrayExpr(af) => {
                use ArrayFunction::*;
                match af {
                    Min | Max => mapper.with_same_dtype(),
                    Sum => mapper.nested_sum_type(),
                    Unique(_) => mapper.try_map_dtype(|dt| {
                        if let DataType::Array(inner, _) = dt {
                            Ok(DataType::List(inner.clone()))
                        } else {
                            polars_bail!(ComputeError: "expected array dtype")
                        }
                    }),
                }
            }
            #[cfg(feature = "dtype-struct")]
            StructExpr(s) => {
                use polars_core::utils::slice_offsets;
                use StructFunction::*;
                match s {
                    FieldByIndex(index) => {
                        let (index, _) = slice_offsets(*index, 0, fields.len());
                        fields.get(index).cloned().ok_or_else(
                            || polars_err!(ComputeError: "index out of bounds in `struct.field`"),
                        )
                    }
                    FieldByName(name) => {
                        if let DataType::Struct(flds) = &fields[0].dtype {
                            let fld = flds
                                .iter()
                                .find(|fld| fld.name() == name.as_ref())
                                .ok_or_else(
                                    || polars_err!(StructFieldNotFound: "{}", name.as_ref()),
                                )?;
                            Ok(fld.clone())
                        } else {
                            polars_bail!(StructFieldNotFound: "{}", name.as_ref());
                        }
                    }
                }
            }
            #[cfg(feature = "top_k")]
            TopK { .. } => mapper.with_same_dtype(),
            Shift(..) | Reverse => mapper.with_same_dtype(),
            Boolean(func) => func.get_field(mapper),
            #[cfg(feature = "dtype-categorical")]
            Categorical(func) => func.get_field(mapper),
            Cumcount { .. } => mapper.with_dtype(IDX_DTYPE),
            Cumsum { .. } => mapper.map_dtype(cum::dtypes::cumsum),
            Cumprod { .. } => mapper.map_dtype(cum::dtypes::cumprod),
            Cummin { .. } => mapper.with_same_dtype(),
            Cummax { .. } => mapper.with_same_dtype(),
            #[cfg(feature = "approx_unique")]
            ApproxUnique => mapper.with_dtype(IDX_DTYPE),
            #[cfg(feature = "diff")]
            Diff(_, _) => mapper.map_dtype(|dt| match dt {
                #[cfg(feature = "dtype-datetime")]
                DataType::Datetime(tu, _) => DataType::Duration(*tu),
                #[cfg(feature = "dtype-date")]
                DataType::Date => DataType::Duration(TimeUnit::Milliseconds),
                #[cfg(feature = "dtype-time")]
                DataType::Time => DataType::Duration(TimeUnit::Nanoseconds),
                DataType::UInt64 | DataType::UInt32 => DataType::Int64,
                DataType::UInt16 => DataType::Int32,
                DataType::UInt8 => DataType::Int16,
                dt => dt.clone(),
            }),
            #[cfg(feature = "interpolate")]
            Interpolate(_) => mapper.with_same_dtype(),
            ShrinkType => {
                // we return the smallest type this can return
                // this might not be correct once the actual data
                // comes in, but if we set the smallest datatype
                // we have the least chance that the smaller dtypes
                // get cast to larger types in type-coercion
                // this will lead to an incorrect schema in polars
                // but we because only the numeric types deviate in
                // bit size this will likely not lead to issues
                mapper.map_dtype(|dt| {
                    if dt.is_numeric() {
                        if dt.is_float() {
                            DataType::Float32
                        } else if dt.is_unsigned() {
                            DataType::Int8
                        } else {
                            DataType::UInt8
                        }
                    } else {
                        dt.clone()
                    }
                })
            }
            #[cfg(feature = "log")]
            Entropy { .. } | Log { .. } | Log1p | Exp => mapper.map_to_float_dtype(),
            Unique(_) => mapper.with_same_dtype(),
            #[cfg(feature = "round_series")]
            Round { .. } | Floor | Ceil => mapper.with_same_dtype(),
            UpperBound | LowerBound => mapper.with_same_dtype(),
            #[cfg(feature = "fused")]
            Fused(_) => mapper.map_to_supertype(),
            ConcatExpr(_) => mapper.map_to_supertype(),
            Correlation { .. } => mapper.map_to_float_dtype(),
            #[cfg(feature = "cutqcut")]
            Cut { .. } => mapper.with_dtype(DataType::Categorical(None)),
            #[cfg(feature = "cutqcut")]
            QCut { .. } => mapper.with_dtype(DataType::Categorical(None)),
            #[cfg(feature = "rle")]
            RLE => mapper.map_dtype(|dt| {
                DataType::Struct(vec![
                    Field::new("lengths", DataType::UInt64),
                    Field::new("values", dt.clone()),
                ])
            }),
            #[cfg(feature = "rle")]
            RLEID => mapper.with_dtype(DataType::UInt32),
            ToPhysical => mapper.to_physical_type(),
            #[cfg(feature = "random")]
            Random { .. } => mapper.with_same_dtype(),
            SetSortedFlag(_) => mapper.with_same_dtype(),
        }
    }
}

pub(super) struct FieldsMapper<'a> {
    fields: &'a [Field],
}

impl<'a> FieldsMapper<'a> {
    /// Field with the same dtype.
    pub(super) fn with_same_dtype(&self) -> PolarsResult<Field> {
        self.map_dtype(|dtype| dtype.clone())
    }

    /// Set a dtype.
    pub(super) fn with_dtype(&self, dtype: DataType) -> PolarsResult<Field> {
        Ok(Field::new(self.fields[0].name(), dtype))
    }

    /// Map a single dtype.
    pub(super) fn map_dtype(&self, func: impl Fn(&DataType) -> DataType) -> PolarsResult<Field> {
        let dtype = func(self.fields[0].data_type());
        Ok(Field::new(self.fields[0].name(), dtype))
    }

    /// Map to a float supertype.
    pub(super) fn map_to_float_dtype(&self) -> PolarsResult<Field> {
        self.map_dtype(|dtype| match dtype {
            DataType::Float32 => DataType::Float32,
            _ => DataType::Float64,
        })
    }

    /// Map to a physical type.
    pub(super) fn to_physical_type(&self) -> PolarsResult<Field> {
        self.map_dtype(|dtype| dtype.to_physical())
    }

    /// Map a single dtype with a potentially failing mapper function.
    #[cfg(any(feature = "timezones", feature = "dtype-array"))]
    pub(super) fn try_map_dtype(
        &self,
        func: impl Fn(&DataType) -> PolarsResult<DataType>,
    ) -> PolarsResult<Field> {
        let dtype = func(self.fields[0].data_type())?;
        Ok(Field::new(self.fields[0].name(), dtype))
    }

    /// Map all dtypes with a potentially failing mapper function.
    pub(super) fn try_map_dtypes(
        &self,
        func: impl Fn(&[&DataType]) -> PolarsResult<DataType>,
    ) -> PolarsResult<Field> {
        let mut fld = self.fields[0].clone();
        let dtypes = self
            .fields
            .iter()
            .map(|fld| fld.data_type())
            .collect::<Vec<_>>();
        let new_type = func(&dtypes)?;
        fld.coerce(new_type);
        Ok(fld)
    }

    /// Map the dtype to the "supertype" of all fields.
    pub(super) fn map_to_supertype(&self) -> PolarsResult<Field> {
        let mut first = self.fields[0].clone();
        let mut st = first.data_type().clone();
        for field in &self.fields[1..] {
            st = try_get_supertype(&st, field.data_type())?
        }
        first.coerce(st);
        Ok(first)
    }

    /// Map the dtype to the dtype of the list elements.
    pub(super) fn map_to_list_inner_dtype(&self) -> PolarsResult<Field> {
        let mut first = self.fields[0].clone();
        let dt = first
            .data_type()
            .inner_dtype()
            .cloned()
            .unwrap_or(DataType::Unknown);
        first.coerce(dt);
        Ok(first)
    }

    #[cfg(feature = "temporal")]
    pub(super) fn map_to_date_range_dtype(
        &self,
        every: &Duration,
        time_unit: &Option<TimeUnit>,
        tz: &Option<String>,
    ) -> PolarsResult<Field> {
        let inner_dtype = match (&self.map_to_supertype()?.dtype, time_unit, tz, every) {
            #[cfg(feature = "timezones")]
            (DataType::Datetime(tu, Some(field_tz)), time_unit, Some(tz), _) => {
                if field_tz != tz {
                    polars_bail!(ComputeError: format!("Given time_zone is different from that of timezone aware datetimes. \
                    Given: '{}', got: '{}'.", tz, field_tz))
                }
                if let Some(time_unit) = time_unit {
                    DataType::Datetime(*time_unit, Some(tz.to_string()))
                } else {
                    DataType::Datetime(*tu, Some(tz.to_string()))
                }
            }
            #[cfg(feature = "timezones")]
            (DataType::Datetime(_, Some(tz)), Some(time_unit), _, _) => {
                DataType::Datetime(*time_unit, Some(tz.to_string()))
            }
            #[cfg(feature = "timezones")]
            (DataType::Datetime(tu, Some(tz)), None, _, _) => {
                DataType::Datetime(*tu, Some(tz.to_string()))
            }
            #[cfg(feature = "timezones")]
            (DataType::Datetime(_, _), Some(time_unit), Some(tz), _) => {
                DataType::Datetime(*time_unit, Some(tz.to_string()))
            }
            #[cfg(feature = "timezones")]
            (DataType::Datetime(tu, _), None, Some(tz), _) => {
                DataType::Datetime(*tu, Some(tz.to_string()))
            }
            (DataType::Datetime(_, _), Some(time_unit), _, _) => {
                DataType::Datetime(*time_unit, None)
            }
            (DataType::Datetime(tu, _), None, _, _) => DataType::Datetime(*tu, None),
            (DataType::Date, time_unit, time_zone, every) => {
                let nsecs = every.nanoseconds();
                if nsecs == 0 {
                    DataType::Date
                } else if let Some(tu) = time_unit {
                    DataType::Datetime(*tu, time_zone.clone())
                } else if nsecs % 1000 != 0 {
                    DataType::Datetime(TimeUnit::Nanoseconds, time_zone.clone())
                } else {
                    DataType::Datetime(TimeUnit::Microseconds, time_zone.clone())
                }
            }
            (dtype, _, _, _) => {
                polars_bail!(ComputeError: "expected Date or Datetime, got {}", dtype)
            }
        };
        Ok(Field::new("date", DataType::List(Box::new(inner_dtype))))
    }

    /// Map the dtypes to the "supertype" of a list of lists.
    pub(super) fn map_to_list_supertype(&self) -> PolarsResult<Field> {
        self.try_map_dtypes(|dts| {
            let mut super_type_inner = None;

            for dt in dts {
                match dt {
                    DataType::List(inner) => match super_type_inner {
                        None => super_type_inner = Some(*inner.clone()),
                        Some(st_inner) => {
                            super_type_inner = Some(try_get_supertype(&st_inner, inner)?)
                        }
                    },
                    dt => match super_type_inner {
                        None => super_type_inner = Some((*dt).clone()),
                        Some(st_inner) => {
                            super_type_inner = Some(try_get_supertype(&st_inner, dt)?)
                        }
                    },
                }
            }
            Ok(DataType::List(Box::new(super_type_inner.unwrap())))
        })
    }

    /// Set the timezone of a datetime dtype.
    #[cfg(feature = "timezones")]
    pub(super) fn map_datetime_dtype_timezone(&self, tz: Option<&TimeZone>) -> PolarsResult<Field> {
        self.try_map_dtype(|dt| {
            if let DataType::Datetime(tu, _) = dt {
                Ok(DataType::Datetime(*tu, tz.cloned()))
            } else {
                polars_bail!(op = "cast-timezone", got = dt, expected = "Datetime");
            }
        })
    }

    fn nested_sum_type(&self) -> PolarsResult<Field> {
        let mut first = self.fields[0].clone();
        use DataType::*;
        let dt = first.data_type().inner_dtype().cloned().unwrap_or(Unknown);

        if matches!(dt, UInt8 | Int8 | Int16 | UInt16) {
            first.coerce(Int64);
        } else {
            first.coerce(dt);
        }
        Ok(first)
    }

    #[cfg(feature = "extract_jsonpath")]
    pub(super) fn with_opt_dtype(&self, dtype: Option<DataType>) -> PolarsResult<Field> {
        let dtype = dtype.unwrap_or(DataType::Unknown);
        self.with_dtype(dtype)
    }
}
