module layered_total_metrics
    use layered_left_metrics, only: doubled
    use layered_right_metrics, only: shifted
    implicit none

    integer, parameter :: total = doubled + shifted
end module layered_total_metrics
